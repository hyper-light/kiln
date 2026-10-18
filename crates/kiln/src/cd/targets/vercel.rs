//! `vercel`'s I/O half (docs/cd-targets.md §3): Vercel's REST API. The pure half,
//! `kiln_cd::providers::vercel`, renders the objects.
//!
//! The production deployment comes from the prebuilt output CI made: its files go to Vercel by
//! SHA-1 (only those Vercel lacks), a deployment is made from them for production, and kiln waits
//! until it is ready. Production reads as kiln's output while the deployment kiln made still
//! serves it; another deployment serving production is drift, or a rollback someone made.

use std::time::Duration;

use anyhow::{Context as _, Result, bail};
use kiln_cd::obj::{self, ResKey};
use kiln_cd::providers::vercel::{self as p, GROUP};
use serde_json::{Map, Value, json};

use super::{Cx, applied, until};
use crate::ci::integrations::aws::encode;
use crate::ci::integrations::vercel::{Vc, VercelError, sha1_hex};
use crate::http::client::Method;

/// Project settings kiln compares and sets.
const SETTINGS: &[&str] = &["framework", "buildCommand", "devCommand", "installCommand", "outputDirectory", "rootDirectory", "nodeVersion", "serverlessFunctionRegion", "publicSource", "commandForIgnoringBuildStep"];
/// The longest a deployment takes to be ready.
const READY: Duration = Duration::from_secs(1800);

fn api<'a>(cx: &'a Cx<'_>) -> Result<Vc<'a>> {
    let (token, config) = cx.token()?;
    Ok(Vc { token, team: config.get("team_id").and_then(Value::as_str) })
}

fn missing<T>(r: Result<T>) -> Result<Option<T>> {
    match r {
        Ok(v) => Ok(Some(v)),
        Err(e) if VercelError::is_not_found(&e) => Ok(None),
        Err(e) => Err(e),
    }
}

async fn project(cx: &Cx<'_>, name: &str) -> Result<Option<Value>> {
    cx.conn.budget().await;
    missing(api(cx)?.call(Method::Get, &format!("/v9/projects/{}", encode(name, false)), None).await)
}

fn project_object(name: &str, p: &Value) -> Value {
    let spec: Map<String, Value> = SETTINGS.iter().filter_map(|k| p.get(*k).filter(|v| !v.is_null()).map(|v| ((*k).to_owned(), v.clone()))).collect();
    let mut o = kiln_cd::providers::object(GROUP, "Project", name, None, Value::Object(spec));
    if let Value::Object(m) = &mut o {
        m.insert("status".into(), json!({ "id": p.get("id"), "updatedAt": p.get("updatedAt") }));
    }
    o
}

/// The project's production deployment as an object.
async fn production(cx: &Cx<'_>, name: &str) -> Result<Option<Value>> {
    let Some(proj) = project(cx, name).await? else { return Ok(None) };
    let Some(id) = proj.pointer("/targets/production/id").and_then(Value::as_str).map(str::to_owned) else { return Ok(None) };
    cx.conn.budget().await;
    let d = api(cx)?.call(Method::Get, &format!("/v13/deployments/{}", encode(&id, false)), None).await?;
    let rec = cx.applied(GROUP, "Production", "", name);
    let output = match rec.as_ref().filter(|r| r.made.get("deployment_id").and_then(Value::as_str) == Some(id.as_str())) {
        Some(r) => r.object.pointer("/spec/output").cloned().unwrap_or(Value::Null),
        None => Value::String(format!("deployment:{id}")),
    };
    let mut o = kiln_cd::providers::object(GROUP, "Production", name, None, json!({ "project": name, "output": output }));
    if let Value::Object(m) = &mut o {
        m.insert("status".into(), json!({ "deploymentId": id, "url": d.get("url"), "readyState": d.get("readyState"), "errorMessage": d.get("errorMessage"), "createdAt": d.get("createdAt") }));
    }
    Ok(Some(o))
}

async fn domain(cx: &Cx<'_>, project: &str, name: &str) -> Result<Option<Value>> {
    cx.conn.budget().await;
    let Some(d) = missing(api(cx)?.call(Method::Get, &format!("/v9/projects/{}/domains/{}", encode(project, false), encode(name, false)), None).await)? else { return Ok(None) };
    let mut o = kiln_cd::providers::object(GROUP, "Domain", name, Some(project), json!({ "project": project }));
    if let Value::Object(m) = &mut o {
        m.insert("status".into(), json!({ "verified": d.get("verified"), "redirect": d.get("redirect") }));
    }
    Ok(Some(o))
}

pub async fn list(cx: &Cx<'_>, kind: &str, names: &[(String, String)]) -> Result<Vec<Value>> {
    let mut out = Vec::new();
    for (ns, name) in names {
        let o = match kind {
            "Project" => project(cx, name).await?.map(|p| project_object(name, &p)),
            "Production" => production(cx, name).await?,
            "Domain" => domain(cx, ns, name).await?,
            _ => None,
        };
        out.extend(o);
    }
    Ok(out)
}

pub async fn apply(cx: &Cx<'_>, desired: &Value) -> Result<Value> {
    match obj::kind(desired) {
        "Project" => apply_project(cx, desired).await,
        "Production" => apply_production(cx, desired).await,
        "Domain" => apply_domain(cx, desired).await,
        other => bail!("vercel doesn't apply {other}"),
    }
}

async fn apply_project(cx: &Cx<'_>, desired: &Value) -> Result<Value> {
    let name = obj::name(desired);
    let settings = desired.get("spec").cloned().unwrap_or(json!({}));
    let vc = api(cx)?;
    let p = match project(cx, name).await? {
        None => {
            let mut body = settings.as_object().cloned().unwrap_or_default();
            body.insert("name".into(), Value::String(name.to_owned()));
            cx.conn.budget().await;
            vc.call(Method::Post, "/v11/projects", Some(&Value::Object(body))).await?
        }
        Some(live) if !kiln_cd::providers::aws_lambda::covers(&settings, &project_object(name, &live).get("spec").cloned().unwrap_or(Value::Null)) => {
            cx.conn.budget().await;
            vc.call(Method::Patch, &format!("/v9/projects/{}", encode(name, false)), Some(&settings)).await?
        }
        Some(live) => live,
    };
    cx.record(GROUP, "Project", "", name, Some(applied(desired, json!({ "id": p.get("id") })))).await?;
    Ok(project_object(name, &p))
}

async fn apply_production(cx: &Cx<'_>, desired: &Value) -> Result<Value> {
    let name = obj::name(desired);
    let want = desired.pointer("/spec/output").and_then(Value::as_str).unwrap_or_default();
    // Already serving it: nothing to make.
    if let Some(live) = production(cx, name).await?
        && live.pointer("/spec/output").and_then(Value::as_str) == Some(want)
    {
        return Ok(live);
    }
    let hex = want.strip_prefix("sha256:").context("spec.output isn't an artifact")?;
    let zip = crate::cd::artifacts::read(cx.files, hex).await?;
    let entries = crate::cd::artifacts::unzip(&zip, crate::cd::artifacts::LIMIT)?;
    let rooted = entries.iter().any(|e| e.name.starts_with(".vercel/output/"));
    if !entries.iter().any(|e| p::output_path(&e.name, rooted) == ".vercel/output/config.json") {
        bail!("the output has no .vercel/output/config.json: it should be what `vercel build` makes");
    }
    let files: Vec<(String, String, Vec<u8>)> = entries.into_iter().map(|e| (p::output_path(&e.name, rooted), sha1_hex(&e.data), e.data)).collect();
    let vc = api(cx)?;
    let body = json!({
        "name": name,
        "project": name,
        "target": "production",
        "files": files.iter().map(|(f, sha, data)| json!({ "file": f, "sha": sha, "size": data.len() })).collect::<Vec<_>>(),
        "meta": { "kiln": obj::annotation(desired, kiln_cd::engine::TRACKING).unwrap_or_default() },
    });
    // Vercel says which files it lacks; those go up, then the deployment is made.
    cx.conn.budget().await;
    let made = match vc.call(Method::Post, "/v13/deployments?skipAutoDetectionConfirmation=1", Some(&body)).await {
        Ok(d) => d,
        Err(e) if e.downcast_ref::<VercelError>().is_some_and(|v| v.code == "missing_files") => {
            let lacks: Vec<String> = e.downcast_ref::<VercelError>().and_then(|v| v.detail.get("missing").and_then(Value::as_array).cloned()).unwrap_or_default().iter().filter_map(|s| s.as_str().map(str::to_owned)).collect();
            for (_, sha, data) in files.iter().filter(|(_, sha, _)| lacks.contains(sha)) {
                cx.conn.budget().await;
                vc.upload(sha, data.clone()).await?;
            }
            cx.conn.budget().await;
            vc.call(Method::Post, "/v13/deployments?skipAutoDetectionConfirmation=1", Some(&body)).await?
        }
        Err(e) => return Err(e),
    };
    let id = made.get("id").and_then(Value::as_str).context("Vercel made no deployment")?.to_owned();
    let ready = until(cx.conn, READY, &format!("deployment {id} getting ready"), || async {
        let d = vc.call(Method::Get, &format!("/v13/deployments/{}", encode(&id, false)), None).await?;
        match d.get("readyState").and_then(Value::as_str) {
            Some("READY") => Ok(Some(d)),
            Some("ERROR") => bail!("deployment {id} failed: {}", d.get("errorMessage").and_then(Value::as_str).unwrap_or("no reason given")),
            Some("CANCELED") => bail!("deployment {id} was canceled"),
            _ => Ok(None),
        }
    })
    .await?;
    cx.record(GROUP, "Production", "", name, Some(applied(desired, json!({ "deployment_id": id, "url": ready.get("url") })))).await?;
    production(cx, name).await?.with_context(|| format!("project {name} has no production deployment"))
}

async fn apply_domain(cx: &Cx<'_>, desired: &Value) -> Result<Value> {
    let (project, name) = (obj::namespace(desired), obj::name(desired));
    if domain(cx, project, name).await?.is_none() {
        cx.conn.budget().await;
        match api(cx)?.call(Method::Post, &format!("/v10/projects/{}/domains", encode(project, false)), Some(&json!({ "name": name }))).await {
            Ok(_) => {}
            Err(e) if e.downcast_ref::<VercelError>().is_some_and(|v| v.code == "domain_already_in_use" || v.status == 409) => bail!("{name} is in use by another project or account"),
            Err(e) => return Err(e),
        }
    }
    cx.record(GROUP, "Domain", project, name, Some(applied(desired, Value::Null))).await?;
    domain(cx, project, name).await?.with_context(|| format!("domain {name} is gone"))
}

pub async fn delete(cx: &Cx<'_>, key: &ResKey) -> Result<()> {
    let vc = api(cx)?;
    cx.conn.budget().await;
    let r = match &*key.kind {
        "Project" => vc.call(Method::Delete, &format!("/v9/projects/{}", encode(&key.name, false)), None).await,
        "Domain" => vc.call(Method::Delete, &format!("/v9/projects/{}/domains/{}", encode(&key.namespace, false), encode(&key.name, false)), None).await,
        // Production is the project's: it goes with the project.
        "Production" => return Ok(()),
        other => bail!("vercel doesn't delete {other}"),
    };
    missing(r).map(|_| ())
}

/// What the account holds: its projects (§8.2, §8.3).
pub async fn discover(cx: &Cx<'_>) -> Result<Value> {
    cx.conn.budget().await;
    let list = api(cx)?.call(Method::Get, "/v9/projects?limit=100", None).await?;
    let out: Vec<Value> = list
        .get("projects")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .map(|p| json!({ "kind": "Project", "name": p.get("name"), "summary": p.get("framework").and_then(Value::as_str).unwrap_or("no framework"), "updated": p.get("updatedAt") }))
        .collect();
    Ok(json!({ "resources": out }))
}

/// A project as it runs, as the definition that adopts it.
pub async fn adopt(cx: &Cx<'_>, name: &str) -> Result<Value> {
    let p = project(cx, name).await?.with_context(|| format!("no project {name} here"))?;
    cx.conn.budget().await;
    let domains = api(cx)?.call(Method::Get, &format!("/v9/projects/{}/domains", encode(name, false)), None).await?;
    let names: Vec<Value> = domains.get("domains").and_then(Value::as_array).into_iter().flatten().filter_map(|d| d.get("name").cloned()).filter(|n| !n.as_str().is_some_and(|s| s.ends_with(".vercel.app"))).collect();
    Ok(json!({
        "definition": { "project": name, "prebuilt": "${{ artifacts.output }}", "settings": project_object(name, &p).get("spec"), "domains": names },
        "note": "point prebuilt at the zip of .vercel/output that `vercel build` makes in CI (an artifact)",
    }))
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use kiln_cd::providers::{RenderInput, Target};

    use super::*;
    use crate::cd::targets::fake::{self, MemFiles, MemRecords};
    use crate::cd::targets::{Conn, Creds, key_of};

    fn desired(hex: &str) -> anyhow::Result<Vec<Value>> {
        let spec = json!({ "project": "site", "prebuilt": format!("sha256:{hex}"), "settings": { "framework": "nextjs" }, "domains": ["www.example.com"] });
        let none = serde_json::Map::new();
        let objs = p::render(&RenderInput { spec: &spec, target: &none, app: "site", namespace: "", revision: "abc", read: &|_| None }).map_err(|e| anyhow::anyhow!("{e}"))?;
        Ok(objs.into_iter().map(|o| fake::tracked(o, "default/site-production")).collect())
    }

    /// A site's life on a faked Vercel: the project made, its production deployed from the
    /// output CI made (only the files Vercel lacks uploaded, the deployment waited on), a domain
    /// added; the same output again makes nothing; a deployment made outside kiln reads as drift.
    #[test]
    fn production_deploys_from_the_prebuilt_output() -> anyhow::Result<()> {
        crate::rt::block_on(async {
            let vercel = Arc::new(fake::FakeVercel::default());
            let base = fake::start_vercel(vercel.clone()).await?;
            crate::ci::integrations::override_endpoint("vercel", &base);
            let target = Target::parse("vercel/vercel").ok_or_else(|| anyhow::anyhow!("target"))?;
            let def = target.def().ok_or_else(|| anyhow::anyhow!("provider"))?;
            let conn = Conn { settle_every: Duration::from_millis(5), gap: Duration::ZERO, ..Conn::new(target, def) };
            let (records, files) = (MemRecords::default(), MemFiles::default());
            let cx = Cx { conn: &conn, creds: Creds::Token { token: "vc-token".into(), config: json!({ "team_id": "team1" }) }, records: &records, files: &files };

            let shared = b"<html>shared</html>".to_vec();
            vercel.state.lock().known.insert(sha1_hex(&shared));
            let hex = files.add(&crate::cd::artifacts::tests::zip(&[("config.json", br#"{"version":3}"#), ("static/index.html", b"<html>v1</html>"), ("static/shared.html", &shared)]));
            let objs = desired(&hex)?;
            for o in &objs {
                apply(&cx, o).await?;
            }
            {
                let st = vercel.state.lock();
                assert_eq!(st.projects.get("site").and_then(|p| p.get("framework")), Some(&json!("nextjs")));
                assert_eq!(st.uploads, 2, "only the files Vercel lacked went up");
                let d = st.deployments.last().cloned().unwrap_or_default();
                assert_eq!(d.get("target"), Some(&json!("production")));
                let names: Vec<&str> = d.get("files").and_then(Value::as_array).map(|f| f.iter().filter_map(|x| x.get("file").and_then(Value::as_str)).collect()).unwrap_or_default();
                assert_eq!(names, [".vercel/output/config.json", ".vercel/output/static/index.html", ".vercel/output/static/shared.html"]);
            }
            let prod = cx.with_applied(list(&cx, "Production", &[(String::new(), "site".to_owned())]).await?.into_iter().next().unwrap_or_default());
            assert_eq!(prod.pointer("/spec/output"), Some(&json!(format!("sha256:{hex}"))));
            assert_eq!(kiln_cd::providers::health(&prod).map(|s| s.health), Some(kiln_cd::model::Health::Healthy));
            assert_eq!(list(&cx, "Domain", &[("site".to_owned(), "www.example.com".to_owned())]).await?.len(), 1);

            // The same output again: no deployment.
            let before = vercel.state.lock().deployments.len();
            apply(&cx, objs.get(1).ok_or_else(|| anyhow::anyhow!("production"))?).await?;
            assert_eq!(vercel.state.lock().deployments.len(), before);

            // A deployment someone made serves production now: drift.
            vercel.state.lock().foreign_production("site");
            let drifted = list(&cx, "Production", &[(String::new(), "site".to_owned())]).await?;
            assert!(drifted.first().and_then(|o| o.pointer("/spec/output")).and_then(Value::as_str).is_some_and(|s| s.starts_with("deployment:")));

            // An output that isn't Vercel's Build Output is refused.
            let bad = files.add(&crate::cd::artifacts::tests::zip(&[("index.html", b"x")]));
            assert!(apply(&cx, desired(&bad)?.get(1).ok_or_else(|| anyhow::anyhow!("production"))?).await.is_err_and(|e| format!("{e:#}").contains("config.json")));

            for o in objs.iter().rev() {
                delete(&cx, &key_of(o)).await?;
            }
            assert!(vercel.state.lock().projects.is_empty());
            Ok(())
        })
    }
}
