//! `gcp-run`'s I/O half (docs/cd-targets.md §3): Cloud Run's Admin API v2. The pure half,
//! `kiln_cd::providers::gcp_run`, renders the objects.
//!
//! A release's template goes to the service under its revision's name, and the traffic to that
//! revision: Cloud Run starts it and moves the traffic once it's ready, which the operation kiln
//! waits for says. A revision the service ran before (a rollback) is there still, so kiln moves
//! the traffic back to it and leaves the template as it is: nothing is made again. Each change
//! goes with the etag kiln read, so one someone made meanwhile isn't overwritten.

use anyhow::{Result, bail};
use kiln_cd::obj::{self, ResKey};
use kiln_cd::providers::gcp_run::{self as p, GROUP};
use serde_json::{Value, json};

use super::gcp::{call, get, missing, wait};
use super::{Cx, applied};
use crate::ci::integrations::gcp::GcpError;
use crate::http::client::Method;

const API: &str = "run";

fn parent(cx: &Cx<'_>) -> Result<String> {
    Ok(format!("projects/{}/locations/{}", cx.place("project")?, cx.place("region")?))
}

async fn service(cx: &Cx<'_>, name: &str) -> Result<Option<Value>> {
    get(cx, API, &format!("/v2/{}/services/{name}", parent(cx)?)).await
}

async fn revision(cx: &Cx<'_>, service: &str, revision: &str) -> Result<Option<Value>> {
    get(cx, API, &format!("/v2/{}/services/{service}/revisions/{revision}", parent(cx)?)).await
}

/// A service as kiln reads it (see [`p::present`]).
async fn live(cx: &Cx<'_>, name: &str) -> Result<Option<Value>> {
    let Some(answer) = service(cx, name).await? else { return Ok(None) };
    let applied = cx.applied(GROUP, "Service", "", name);
    Ok(p::live_service(&answer).map(|l| p::present(l, applied.as_ref().map(|a| &a.object))))
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

/// What the project holds in the region: its services (§8.2, §8.3).
pub async fn discover(cx: &Cx<'_>) -> Result<Value> {
    let mut out = Vec::new();
    let mut token = String::new();
    loop {
        let mut q = vec![("pageSize", "100")];
        if !token.is_empty() {
            q.push(("pageToken", token.as_str()));
        }
        let page = call(cx, API, Method::Get, &format!("/v2/{}/services", parent(cx)?), &q, None).await?;
        for s in page.get("services").and_then(Value::as_array).into_iter().flatten() {
            let name = p::short(s.get("name").and_then(Value::as_str).unwrap_or_default()).to_owned();
            let image = s.pointer("/template/containers/0/image").and_then(Value::as_str).unwrap_or_default();
            let url = s.get("uri").and_then(Value::as_str).unwrap_or_default();
            out.push(json!({ "kind": "Service", "name": name, "summary": format!("{image} · {url}"), "updated": s.get("updateTime"), "description": s.get("description") }));
        }
        token = page.get("nextPageToken").and_then(Value::as_str).unwrap_or_default().to_owned();
        if token.is_empty() || out.len() >= 1000 {
            break;
        }
    }
    Ok(json!({ "resources": out }))
}

/// A service as it runs now, as the definition that adopts it (§8.2 step 2).
pub async fn adopt(cx: &Cx<'_>, name: &str) -> Result<Value> {
    let Some(live) = live(cx, name).await? else { bail!("no service {name} here") };
    let mut spec = live.get("spec").cloned().unwrap_or_else(|| json!({}));
    let image = spec.pointer("/template/containers/0/image").cloned();
    if let Some(m) = spec.as_object_mut() {
        m.remove("traffic");
        if let Some(t) = m.get_mut("template").and_then(Value::as_object_mut) {
            t.remove("revision");
        }
    }
    if let Some(c) = spec.pointer_mut("/template/containers/0").and_then(Value::as_object_mut) {
        c.remove("image");
    }
    let def = json!({ "service": name, "image": image, "spec": spec });
    Ok(json!({ "definition": def, "note": "kiln names each release's revision for its template, and gives it all the traffic", "live": live }))
}

/// Applies a Service: its template under its revision's name unless that revision is there, the
/// traffic to it, then waits for Cloud Run to roll it out.
pub async fn apply(cx: &Cx<'_>, desired: &Value) -> Result<Value> {
    let name = obj::name(desired);
    let want = desired.pointer("/spec/template/revision").and_then(Value::as_str).ok_or_else(|| anyhow::anyhow!("service {name} names no revision"))?.to_owned();
    let what = format!("rolling out {name}'s revision {want}");
    let op = match service(cx, name).await? {
        None => Some(call(cx, API, Method::Post, &format!("/v2/{}/services", parent(cx)?), &[("serviceId", name)], Some(&p::create_body(desired))).await?),
        Some(answer) => {
            let current = answer.pointer("/template/revision").and_then(Value::as_str).map(p::short);
            // The template is sent only for a revision that isn't there: Cloud Run keeps every
            // revision, and one named again with other content is refused.
            let keep = current == Some(want.as_str()) || revision(cx, name, &want).await?.is_some();
            let body = p::update_body(&answer, desired, keep);
            if p::unchanged(&answer, &body) {
                None
            } else {
                let path = format!("/v2/{}/services/{name}", parent(cx)?);
                Some(call(cx, API, Method::Patch, &path, &[], Some(&body)).await.map_err(|e| match e.downcast_ref::<GcpError>() {
                    Some(g) if g.code == "ABORTED" || g.code == "FAILED_PRECONDITION" => anyhow::anyhow!("service {name} changed meanwhile ({g}): kiln reads it again and retries"),
                    _ => e,
                })?)
            }
        }
    };
    cx.record(GROUP, "Service", "", name, Some(applied(desired, json!({ "revision": want })))).await?;
    if let Some(op) = op {
        wait(cx, API, "v2", op, &what).await?;
    }
    live(cx, name).await?.ok_or_else(|| anyhow::anyhow!("service {name} is gone"))
}

pub async fn delete(cx: &Cx<'_>, key: &ResKey) -> Result<()> {
    if &*key.kind != "Service" {
        bail!("gcp-run doesn't delete {}", key.kind);
    }
    let path = format!("/v2/{}/services/{}", parent(cx)?, key.name);
    if let Some(op) = missing(call(cx, API, Method::Delete, &path, &[], None).await)? {
        wait(cx, API, "v2", op, &format!("deleting service {}", key.name)).await?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use kiln_cd::model::Health;
    use kiln_cd::providers::Target;

    use super::*;
    use crate::cd::targets::fake::{MemFiles, MemRecords};
    use crate::cd::targets::fake_gcp::{self, FakeGcp};
    use crate::cd::targets::fake_query::released_to;
    use crate::cd::targets::{Conn, Creds, key_of};

    fn spec() -> Value {
        json!({ "service": "shop", "image": "us-docker.pkg.dev/acme/shop/web@${{ release.artifacts.image.digest }}",
            "spec": { "ingress": "INGRESS_TRAFFIC_ALL", "template": { "scaling": { "maxInstanceCount": 4 }, "containers": [{ "env": [{ "name": "MODE", "value": "${{ env.name }}" }] }] } } })
    }

    /// A service on a faked Cloud Run, release by release: the first makes the service and a
    /// revision of its template, and the traffic goes to it once it has started; the same release
    /// again changes nothing; the next makes its own revision, which takes the traffic; one whose
    /// container doesn't start fails, and the traffic stays where it was; the release before,
    /// rolled back to, moves the traffic back to its revision and starts nothing; traffic someone
    /// moved reads as drift, and goes back; then the service goes.
    #[test]
    fn services_run_each_releases_revision() -> anyhow::Result<()> {
        let _one = fake_gcp::GCP.lock();
        crate::rt::block_on(async {
            let gcp = Arc::new(FakeGcp::default());
            fake_gcp::start(gcp.clone()).await?;
            let target = Target::parse("gcp-run/gcp-test/acme-prod/us-central1").ok_or_else(|| anyhow::anyhow!("target"))?;
            let def = target.def().ok_or_else(|| anyhow::anyhow!("provider"))?;
            assert!(def.built);
            let conn = Conn::quick(target, def);
            let (records, files) = (MemRecords::default(), MemFiles::default());
            let cx = Cx { conn: &conn, creds: Creds::Bearer(fake_gcp::TOKEN.into()), records: &records, files: &files };
            let place = json!({ "account": "gcp-test", "project": "acme-prod", "region": "us-central1" });
            let release = |version: &str, digest: &str| -> anyhow::Result<Value> {
                released_to("gcp-run", &place, spec(), version, &[], &[("image", digest)])?.into_iter().next().ok_or_else(|| anyhow::anyhow!("no service"))
            };
            let names = [(String::new(), "shop".to_owned())];
            let read = || async { Ok::<_, anyhow::Error>(list(&cx, "Service", &names).await?.into_iter().next().unwrap_or_default()) };
            let h = |o: &Value| kiln_cd::providers::health(o).map(|s| s.health);
            let revision_of = |o: &Value| o.pointer("/spec/template/revision").and_then(Value::as_str).unwrap_or_default().to_owned();
            let last_call = || gcp.state.lock().calls.last().cloned().unwrap_or_default();
            // As the engine compares: what kiln sets, against the object as it reads.
            let synced = |desired: &Value, live: &Value| {
                let mut d = desired.clone();
                kiln_cd::diff::normalize_desired(&mut d);
                matches!(kiln_cd::diff::compare(&d, live, None), kiln_cd::diff::Verdict::Synced)
            };

            // The first release: the service, its revision, and all the traffic once it started.
            let svc1 = release("1.0.0", "sha256:aaaa")?;
            let rev1 = revision_of(&svc1);
            assert_eq!(svc1.pointer("/spec/template/containers/0/env/0/value"), Some(&json!("staging")));
            let up = apply(&cx, &svc1).await?;
            assert_eq!(h(&up), Some(Health::Healthy), "{up}");
            assert_eq!(gcp.state.lock().calls, [format!("CreateService shop revision={rev1} traffic={rev1}:100")]);
            assert_eq!(up.pointer("/spec/template"), svc1.pointer("/spec/template"), "Cloud Run's defaults aren't drift: {up}");
            assert_eq!(up.pointer("/spec/traffic"), svc1.pointer("/spec/traffic"));
            assert!(synced(&svc1, &up), "in sync: {up}");
            assert_eq!(obj::label(&up, kiln_cd::engine::APP_LABEL), obj::label(&svc1, kiln_cd::engine::APP_LABEL), "tracking in the service's labels");

            // The same release again: nothing changes.
            apply(&cx, &release("1.0.0", "sha256:aaaa")?).await?;
            assert_eq!(gcp.state.lock().calls.len(), 1);

            // The next: a revision of its own, which takes the traffic once it has started.
            let svc2 = release("1.1.0", "sha256:bbbb")?;
            let rev2 = revision_of(&svc2);
            assert_ne!(rev1, rev2);
            let up2 = apply(&cx, &svc2).await?;
            assert_eq!((p::serving(&up2), h(&up2)), (Some(rev2.clone()), Some(Health::Healthy)), "{up2}");
            assert_eq!(last_call(), format!("UpdateService shop revision={rev2} traffic={rev2}:100"));

            // One whose container doesn't start: its apply fails, the traffic stays where it was,
            // and the service reads as degraded.
            let svc3 = release("1.2.0", "sha256:broken")?;
            let failed = apply(&cx, &svc3).await.err().map(|e| format!("{e:#}")).unwrap_or_default();
            assert!(failed.contains("is not ready and cannot serve traffic"), "{failed}");
            let down = read().await?;
            assert_eq!(h(&down), Some(Health::Degraded), "{down}");
            assert_eq!(down.pointer("/status/trafficStatuses/0/revision"), Some(&json!(rev2)), "the traffic stayed: {down}");

            // Rolled back to the release before: its revision is there, so only the traffic moves.
            let made = gcp.state.lock().run.get("shop").map_or(0, |s| s.revisions.len());
            let back = apply(&cx, &release("1.1.0", "sha256:bbbb")?).await?;
            assert_eq!(gcp.state.lock().run.get("shop").map_or(0, |s| s.revisions.len()), made, "no revision made for a rollback");
            assert_eq!(last_call(), format!("UpdateService shop traffic={rev2}:100"));
            assert_eq!(h(&back), Some(Health::Healthy), "the failed revision holds no traffic: {back}");
            assert_eq!(back.pointer("/spec/template"), svc2.pointer("/spec/template"), "it reads as the release's: {back}");

            // Someone sends the traffic to the newest revision in the console: drift, read as theirs;
            // kiln's next apply puts it back.
            gcp.state.lock().follow_latest("shop");
            let moved = read().await?;
            assert_eq!(moved.pointer("/spec/traffic/0/type"), Some(&json!(p::LATEST)), "{moved}");
            assert!(!synced(&svc2, &moved), "drift");
            apply(&cx, &svc2).await?;
            assert_eq!(last_call(), format!("UpdateService shop traffic={rev2}:100"));

            // What the project holds, and the definition that adopts it; then it goes.
            let found = discover(&cx).await?;
            assert_eq!(found.pointer("/resources/0/name"), Some(&json!("shop")));
            let adopted = adopt(&cx, "shop").await?;
            assert_eq!(adopted.pointer("/definition/image"), Some(&json!("us-docker.pkg.dev/acme/shop/web@sha256:bbbb")), "what serves: {adopted}");
            delete(&cx, &key_of(&svc2)).await?;
            assert!(list(&cx, "Service", &names).await?.is_empty(), "deleted");
            assert_eq!(last_call(), "DeleteService shop");
            Ok(())
        })
    }
}
