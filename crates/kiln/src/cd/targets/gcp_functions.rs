//! `gcp-functions`' I/O half (docs/cd-targets.md §3): Cloud Functions' v2 API. The pure half,
//! `kiln_cd::providers::gcp_functions`, renders the objects.
//!
//! A zip CI built goes to the URL Cloud Functions signs for an upload (`generateUploadUrl`), the
//! function is made or changed to build from it, and kiln waits for the build and the rollout
//! (the operation). The same release again uploads nothing and changes nothing: kiln keeps where
//! its upload went, and the function still builds from there.

use anyhow::{Result, bail};
use kiln_cd::obj::{self, ResKey};
use kiln_cd::providers::gcp_functions::{self as p, GROUP};
use kiln_cd::providers::gcp_run::{covers, short};
use serde_json::{Value, json};

use super::gcp::{call, get, missing, wait};
use super::{Cx, applied};
use crate::http::client::Method;

const API: &str = "cloudfunctions";

fn parent(cx: &Cx<'_>) -> Result<String> {
    Ok(format!("projects/{}/locations/{}", cx.place("project")?, cx.place("region")?))
}

async fn function(cx: &Cx<'_>, name: &str) -> Result<Option<Value>> {
    get(cx, API, &format!("/v2/{}/functions/{name}", parent(cx)?)).await
}

async fn live(cx: &Cx<'_>, name: &str) -> Result<Option<Value>> {
    let Some(answer) = function(cx, name).await? else { return Ok(None) };
    let rec = cx.applied(GROUP, "Function", "", name);
    Ok(p::live_function(&answer, rec.as_ref().map(|r| (&r.object, &r.made))))
}

pub async fn list(cx: &Cx<'_>, kind: &str, names: &[(String, String)]) -> Result<Vec<Value>> {
    let mut out = Vec::new();
    for (_, name) in names {
        if kind == "Function" {
            out.extend(live(cx, name).await?);
        }
    }
    Ok(out)
}

/// What the project holds in the region: its functions (§8.2, §8.3).
pub async fn discover(cx: &Cx<'_>) -> Result<Value> {
    let mut out = Vec::new();
    let mut token = String::new();
    loop {
        let mut q = vec![("pageSize", "100")];
        if !token.is_empty() {
            q.push(("pageToken", token.as_str()));
        }
        let page = call(cx, API, Method::Get, &format!("/v2/{}/functions", parent(cx)?), &q, None).await?;
        for f in page.get("functions").and_then(Value::as_array).into_iter().flatten() {
            let s = |k: &str| f.pointer(k).and_then(Value::as_str).unwrap_or_default();
            out.push(json!({ "kind": "Function", "name": short(s("/name")), "summary": format!("{} · {}", s("/buildConfig/runtime"), s("/state")), "updated": s("/updateTime"), "description": s("/description") }));
        }
        token = page.get("nextPageToken").and_then(Value::as_str).unwrap_or_default().to_owned();
        if token.is_empty() || out.len() >= 1000 {
            break;
        }
    }
    Ok(json!({ "resources": out }))
}

/// A function as it runs now, as the definition that adopts it (§8.2 step 2).
pub async fn adopt(cx: &Cx<'_>, name: &str) -> Result<Value> {
    let Some(live) = live(cx, name).await? else { bail!("no function {name} here") };
    let spec = live.get("spec").cloned().unwrap_or_else(|| json!({}));
    let s = |k: &str| spec.pointer(k).cloned().unwrap_or(Value::Null);
    let mut rest = spec.clone();
    if let Some(b) = rest.get_mut("buildConfig").and_then(Value::as_object_mut) {
        for k in ["runtime", "entryPoint", "source"] {
            b.remove(k);
        }
    }
    let def = json!({ "function": name, "runtime": s("/buildConfig/runtime"), "entry-point": s("/buildConfig/entryPoint"), "source": "${{ release.artifacts.source.digest }}", "spec": rest });
    Ok(json!({ "definition": def, "note": "Cloud Functions doesn't say where a function's source came from: point source at the zip CI builds (an artifact), or a gs:// object", "live": live }))
}

/// Uploads the zip CI built (by its SHA-256, in hex) to a URL Cloud Functions signs for it:
/// where it went (`storageSource`).
async fn upload(cx: &Cx<'_>, hex: &str) -> Result<Value> {
    let place = call(cx, API, Method::Post, &format!("/v2/{}/functions:generateUploadUrl", parent(cx)?), &[], Some(&json!({}))).await?;
    let url = place.get("uploadUrl").and_then(Value::as_str).ok_or_else(|| anyhow::anyhow!("Cloud Functions gave no upload URL"))?;
    let storage = place.get("storageSource").cloned().ok_or_else(|| anyhow::anyhow!("Cloud Functions said nowhere for the upload"))?;
    let (rx, len) = cx.files.open(hex).await?;
    cx.conn.budget().await;
    cx.gcp()?.upload(Method::Put, url, &[("Content-Type", "application/zip")], rx, len, false).await?;
    Ok(storage)
}

/// Applies a Function: its source uploaded unless the function builds from that upload already,
/// then the function made or changed, and its build and rollout waited for.
pub async fn apply(cx: &Cx<'_>, desired: &Value) -> Result<Value> {
    let name = obj::name(desired);
    let current = function(cx, name).await?;
    let source = desired.pointer("/spec/buildConfig/source").cloned().unwrap_or(Value::Null);
    let (artifact, storage) = match source.get("artifact").and_then(Value::as_str) {
        Some(a) => {
            let uploaded = cx.applied(GROUP, "Function", "", name).filter(|r| r.made.get("artifact").and_then(Value::as_str) == Some(a)).and_then(|r| r.made.get("storageSource").cloned());
            let have = current.as_ref().and_then(|c| c.pointer("/buildConfig/source/storageSource"));
            let kept = uploaded.filter(|u| have.is_some_and(|h| p::same_upload(h, u)));
            let storage = match kept {
                Some(s) => s,
                None => upload(cx, a.trim_start_matches("sha256:")).await?,
            };
            (Some(a.to_owned()), storage)
        }
        None => (None, source.get("storageSource").cloned().ok_or_else(|| anyhow::anyhow!("function {name} has no source"))?),
    };
    let body = p::function_body(current.as_ref(), desired, &storage);
    let op = match &current {
        None => Some(call(cx, API, Method::Post, &format!("/v2/{}/functions", parent(cx)?), &[("functionId", name)], Some(&body)).await?),
        Some(c) if covers(c, &body) => None,
        Some(_) => Some(call(cx, API, Method::Patch, &format!("/v2/{}/functions/{name}", parent(cx)?), &[], Some(&body)).await?),
    };
    cx.record(GROUP, "Function", "", name, Some(applied(desired, json!({ "artifact": artifact, "storageSource": storage })))).await?;
    if let Some(op) = op {
        wait(cx, API, "v2", op, &format!("deploying function {name}")).await?;
    }
    live(cx, name).await?.ok_or_else(|| anyhow::anyhow!("function {name} is gone"))
}

pub async fn delete(cx: &Cx<'_>, key: &ResKey) -> Result<()> {
    if &*key.kind != "Function" {
        bail!("gcp-functions doesn't delete {}", key.kind);
    }
    let path = format!("/v2/{}/functions/{}", parent(cx)?, key.name);
    if let Some(op) = missing(call(cx, API, Method::Delete, &path, &[], None).await)? {
        wait(cx, API, "v2", op, &format!("deleting function {}", key.name)).await?;
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
        json!({ "function": "thumbs", "source": "${{ release.artifacts.source.digest }}", "runtime": "nodejs22", "entry-point": "handler",
            "spec": { "serviceConfig": { "availableMemory": "512M", "environmentVariables": { "MODE": "${{ env.name }}" } } } })
    }

    /// A function on a faked Cloud Functions, release by release: the first uploads its zip to a
    /// URL Cloud Functions signed and makes the function, which builds it and serves it; the same
    /// release again uploads nothing and changes nothing; the next uploads and builds its own; one
    /// whose build fails fails, the revision before serving still; the release before, rolled
    /// back to, deploys its source again; a source someone else deployed reads as drift; then the
    /// function goes.
    #[test]
    fn functions_build_each_releases_source() -> anyhow::Result<()> {
        let _one = fake_gcp::GCP.lock();
        crate::rt::block_on(async {
            let gcp = Arc::new(FakeGcp::default());
            fake_gcp::start(gcp.clone()).await?;
            let (target, place) = fake_gcp::target("gcp-functions")?;
            let def = target.def().ok_or_else(|| anyhow::anyhow!("provider"))?;
            let conn = Conn::quick(target, def);
            let (records, files) = (MemRecords::default(), MemFiles::default());
            let cx = Cx { conn: &conn, creds: Creds::Bearer(fake_gcp::TOKEN.into()), records: &records, files: &files };
            let release = |version: &str, zip: &str| -> anyhow::Result<Value> {
                let digest = format!("sha256:{}", files.add(zip.as_bytes()));
                released_to("gcp-functions", &place, spec(), version, &[], &[("source", &digest)])?.into_iter().next().ok_or_else(|| anyhow::anyhow!("no function"))
            };
            let names = [(String::new(), "thumbs".to_owned())];
            let read = || async { Ok::<_, anyhow::Error>(list(&cx, "Function", &names).await?.into_iter().next().unwrap_or_default()) };
            let h = |o: &Value| kiln_cd::providers::health(o).map(|s| s.health);
            let uploads = || gcp.state.lock().calls.iter().filter(|c| c.starts_with("UploadSource")).count();
            let changes = || gcp.state.lock().calls.iter().filter(|c| c.contains("Function ")).count();

            // The first release: its zip uploaded, the function made, built and serving.
            let f1 = release("1.0.0", "zip of thumbs 1.0.0")?;
            assert_eq!(f1.pointer("/spec/serviceConfig/environmentVariables/MODE"), Some(&json!("staging")));
            let up = apply(&cx, &f1).await?;
            assert_eq!(h(&up), Some(Health::Healthy), "{up}");
            assert!(synced(&f1, &cx.with_applied(up.clone())), "in sync: {up}");
            assert_eq!((uploads(), changes()), (1, 1));
            assert!(gcp.state.lock().calls.last().is_some_and(|c| c.starts_with("CreateFunction thumbs source=gcf-v2-uploads-")));
            assert_eq!(up.pointer("/status/revision"), Some(&json!("thumbs-00001-abc")));
            assert_eq!(obj::label(&up, kiln_cd::engine::APP_LABEL), obj::label(&f1, kiln_cd::engine::APP_LABEL), "tracking in the function's labels");

            // The same release again: nothing uploaded, nothing changed.
            apply(&cx, &release("1.0.0", "zip of thumbs 1.0.0")?).await?;
            assert_eq!((uploads(), changes()), (1, 1));

            // The next: uploaded, built, and serving its own revision.
            let f2 = release("1.1.0", "zip of thumbs 1.1.0")?;
            let up2 = apply(&cx, &f2).await?;
            assert_eq!((uploads(), changes(), h(&up2)), (2, 2, Some(Health::Healthy)));
            assert_eq!(up2.pointer("/status/revision"), Some(&json!("thumbs-00002-abc")));

            // One whose build fails: its apply fails, and the revision before serves still.
            let failed = apply(&cx, &release("1.2.0", "a broken zip")?).await.err().map(|e| format!("{e:#}")).unwrap_or_default();
            assert!(failed.contains("Build failed"), "{failed}");
            let down = read().await?;
            assert_eq!(h(&down), Some(Health::Degraded), "{down}");
            assert_eq!(down.pointer("/status/revision"), Some(&json!("thumbs-00002-abc")));

            // Rolled back to the release before: its source deployed again, and active.
            let back = apply(&cx, &release("1.1.0", "zip of thumbs 1.1.0")?).await?;
            assert_eq!((uploads(), h(&back)), (4, Some(Health::Healthy)), "{back}");
            assert!(synced(&f2, &cx.with_applied(back.clone())), "it reads as the release's: {back}");

            // Someone deploys another source from the console: drift, read as theirs.
            if let Some(src) = gcp.state.lock().functions.get_mut("thumbs").and_then(|f| f.body.get_mut("buildConfig")).and_then(|b| b.pointer_mut("/source/storageSource")).and_then(Value::as_object_mut) {
                src.insert("object".into(), json!("from-the-console.zip"));
            }
            let theirs = read().await?;
            assert_eq!(theirs.pointer("/spec/buildConfig/source/storageSource/object"), Some(&json!("from-the-console.zip")));
            assert!(!synced(&f2, &cx.with_applied(theirs.clone())), "drift");

            // What the project holds, and the definition that adopts it; then it goes.
            let found = discover(&cx).await?;
            assert_eq!(found.pointer("/resources/0/name"), Some(&json!("thumbs")));
            let adopted = adopt(&cx, "thumbs").await?;
            assert_eq!(adopted.pointer("/definition/runtime"), Some(&json!("nodejs22")));
            delete(&cx, &key_of(&f2)).await?;
            assert!(list(&cx, "Function", &names).await?.is_empty(), "deleted");
            Ok(())
        })
    }
}
