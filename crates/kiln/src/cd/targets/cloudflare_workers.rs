//! `cloudflare-workers`' I/O half (docs/cd-targets.md §3): the Workers API. The pure half,
//! `kiln_cd::providers::cloudflare_workers`, reads the wrangler config into objects.
//!
//! A Worker's apply uploads the bundle's modules with their metadata, the Durable Object
//! migration steps after the tag the Worker has, and kiln's tag. An upload deploys at once, at
//! 100%: a version with a migration is never split. A Worker that already runs what kiln wants is
//! left alone (an upload is a new version). The live Worker reads its bundle as what kiln
//! uploaded while the version kiln deployed is the one serving; another version is drift.
//!
//! An app's secrets (docs/deploy-api.md §3.5) are `secret_text` bindings of the upload, their
//! values read from kiln's secrets service as it uploads; they read back as kiln's references
//! while the version kiln deployed serves (Cloudflare never answers their text), and one the app
//! no longer has is deleted. Secrets set outside kiln are kept across uploads.

use anyhow::{Context as _, Result, bail};
use kiln_cd::obj::{self, ResKey};
use kiln_cd::providers::cloudflare_workers::{self as p, GROUP};
use serde_json::{Map, Value, json};

use super::{Cx, applied};
use kiln_cd::apps::{SECRET_CHANGED, SECRET_REF};
use crate::ci::integrations::aws::encode;
use crate::ci::integrations::cloudflare::{Cf, CfError, Part};
use crate::http::client::Method;

/// The Workers API for the target's account.
fn api<'a>(cx: &'a Cx<'_>) -> Result<(Cf<'a>, String)> {
    let (token, config) = cx.token()?;
    let account = config.get("account_id").and_then(Value::as_str).filter(|a| !a.is_empty()).context("the Cloudflare account's integration has no account_id")?;
    Ok((Cf { token }, account.to_owned()))
}

/// The tag kiln marks a Worker with: its app label.
const TAG: &str = "kiln.dev/app=";

/// Every Worker of the account, by name: its migration tag and its tags.
async fn scripts(cx: &Cx<'_>) -> Result<Map<String, Value>> {
    let (cf, account) = api(cx)?;
    cx.conn.budget().await;
    let list = cf.call(Method::Get, &format!("/accounts/{account}/workers/scripts"), None).await?;
    Ok(list.as_array().into_iter().flatten().filter_map(|s| Some((s.get("id")?.as_str()?.to_owned(), s.clone()))).collect())
}

/// A Worker as an object: its settings, schedules, subdomain and deployment, with what kiln
/// uploaded read back while the version it deployed still serves.
async fn worker(cx: &Cx<'_>, name: &str, script: &Value) -> Result<Value> {
    let (cf, account) = api(cx)?;
    let base = format!("/accounts/{account}/workers/scripts/{}", encode(name, false));
    cx.conn.budget().await;
    let settings = cf.call(Method::Get, &format!("{base}/settings"), None).await?;
    cx.conn.budget().await;
    let deployments = cf.call(Method::Get, &format!("{base}/deployments"), None).await?;
    cx.conn.budget().await;
    let schedules = cf.call(Method::Get, &format!("{base}/schedules"), None).await.unwrap_or(Value::Null);
    cx.conn.budget().await;
    let subdomain = cf.call(Method::Get, &format!("{base}/subdomain"), None).await.unwrap_or(Value::Null);
    let deployment = deployments.get("deployments").and_then(Value::as_array).and_then(|d| d.first()).cloned().unwrap_or(Value::Null);
    let versions = deployment.get("versions").and_then(Value::as_array).cloned().unwrap_or_default();
    let serving = match versions.as_slice() {
        [only] if only.get("percentage").and_then(Value::as_f64).unwrap_or(100.0) >= 100.0 => only.get("version_id").and_then(Value::as_str).map(str::to_owned),
        _ => None,
    };
    let rec = cx.applied(GROUP, "Worker", "", name);
    let ours = rec.as_ref().filter(|r| serving.is_some() && r.made.get("version_id").and_then(Value::as_str) == serving.as_deref());
    let mut spec = Map::new();
    match ours {
        Some(r) => {
            for k in ["bundle", "main_module"] {
                if let Some(v) = r.object.pointer(&format!("/spec/{k}")) {
                    spec.insert(k.into(), v.clone());
                }
            }
        }
        None => {
            spec.insert("bundle".into(), Value::String(match &serving {
                Some(v) => format!("version:{v}"),
                None => format!("deployment:{}", deployment.get("id").and_then(Value::as_str).unwrap_or("none")),
            }));
        }
    }
    for k in ["compatibility_date", "compatibility_flags", "observability", "placement", "limits", "logpush", "tail_consumers"] {
        if let Some(v) = settings.get(k).filter(|v| !v.is_null()) {
            spec.insert(k.into(), v.clone());
        }
    }
    // Secrets are set apart from the bundle, and kept across uploads: not kiln's to compare, but
    // for the app's own, which read as kiln uploaded them while its version serves.
    let all: Vec<Value> = settings.get("bindings").and_then(Value::as_array).cloned().unwrap_or_default();
    let secret = |b: &Value| matches!(b.get("type").and_then(Value::as_str), Some("secret_text" | "secret_key"));
    let mut bindings: Vec<Value> = all.iter().filter(|b| !secret(b)).cloned().collect();
    for b in kiln_secrets(rec.as_ref().map(|r| &r.object)) {
        let name = b.get("name").and_then(Value::as_str);
        if all.iter().any(|l| secret(l) && l.get("name").and_then(Value::as_str) == name) {
            bindings.push(if ours.is_some() { b.clone() } else { json!({ "type": "secret_text", "name": name, "text": SECRET_CHANGED }) });
        }
    }
    p::sort_bindings(&mut bindings);
    spec.insert("bindings".into(), Value::Array(bindings));
    // Migrations read as kiln applied them while the Worker is at the tag they end at.
    let tag = script.get("migration_tag").and_then(Value::as_str).map(str::to_owned);
    let applied_tag = rec.as_ref().and_then(|r| r.made.get("migration_tag").and_then(Value::as_str).map(str::to_owned));
    match (&tag, rec.as_ref().and_then(|r| r.object.pointer("/spec/migrations"))) {
        (Some(t), Some(m)) if applied_tag.as_deref() == Some(t) => {
            spec.insert("migrations".into(), m.clone());
        }
        (Some(t), _) => {
            spec.insert("migrations".into(), json!([{ "tag": t }]));
        }
        _ => {}
    }
    spec.insert("crons".into(), Value::Array(schedules.get("schedules").and_then(Value::as_array).into_iter().flatten().filter_map(|s| s.get("cron").cloned()).collect()));
    if let Some(e) = subdomain.get("enabled") {
        spec.insert("workers_dev".into(), e.clone());
    }
    let mut o = kiln_cd::providers::object(GROUP, "Worker", name, None, Value::Object(spec));
    if let Value::Object(m) = &mut o {
        m.insert("status".into(), json!({ "deployment": deployment, "migration_tag": tag, "modified_on": script.get("modified_on"), "etag": script.get("etag") }));
    }
    let label = script.get("tags").and_then(Value::as_array).into_iter().flatten().filter_map(Value::as_str).find_map(|t| t.strip_prefix(TAG)).map(str::to_owned);
    if let (Some(l), Some(labels)) = (label, obj::ensure_map(&mut o, &["metadata", "labels"])) {
        labels.insert(kiln_cd::engine::APP_LABEL.into(), Value::String(l));
    }
    Ok(o)
}

/// The app's secrets among a Worker object's bindings: kiln's references.
fn kiln_secrets(o: Option<&Value>) -> Vec<Value> {
    o.and_then(|o| o.pointer("/spec/bindings")).and_then(Value::as_array).into_iter().flatten().filter(|b| b.get("text").and_then(Value::as_str).is_some_and(|t| t.starts_with(SECRET_REF))).cloned().collect()
}

async fn zone_id(cx: &Cx<'_>, zone: &str) -> Result<String> {
    let memo = format!("cf:zone:{zone}");
    if let Some(z) = cx.conn.memo.lock().get(&memo).cloned() {
        return Ok(z);
    }
    let (cf, _) = api(cx)?;
    cx.conn.budget().await;
    let zones = cf.call(Method::Get, &format!("/zones?name={}", encode(zone, false)), None).await?;
    let id = zones.as_array().and_then(|z| z.first()).and_then(|z| z.get("id")).and_then(Value::as_str).with_context(|| format!("no zone {zone} in this account"))?.to_owned();
    cx.conn.memo.lock().insert(memo, id.clone());
    Ok(id)
}

/// A zone's routes.
async fn routes(cx: &Cx<'_>, zone: &str) -> Result<(String, Vec<Value>)> {
    let id = zone_id(cx, zone).await?;
    let (cf, _) = api(cx)?;
    cx.conn.budget().await;
    let list = cf.call(Method::Get, &format!("/zones/{id}/workers/routes"), None).await?;
    Ok((id, list.as_array().cloned().unwrap_or_default()))
}

/// The account's custom domain for `hostname`, if any.
async fn domain(cx: &Cx<'_>, hostname: &str) -> Result<Option<Value>> {
    let (cf, account) = api(cx)?;
    cx.conn.budget().await;
    let list = cf.call(Method::Get, &format!("/accounts/{account}/workers/domains?hostname={}", encode(hostname, false)), None).await?;
    Ok(list.as_array().and_then(|l| l.iter().find(|d| d.get("hostname").and_then(Value::as_str) == Some(hostname))).cloned())
}

pub async fn list(cx: &Cx<'_>, kind: &str, names: &[(String, String)]) -> Result<Vec<Value>> {
    let mut out = Vec::new();
    match kind {
        "Worker" => {
            let all = scripts(cx).await?;
            for (_, name) in names {
                if let Some(s) = all.get(name) {
                    out.push(worker(cx, name, s).await?);
                }
            }
        }
        "Route" => {
            // A route's zone is in what kiln applied, or the pattern's host.
            for (_, pattern) in names {
                let zone = cx.applied(GROUP, "Route", "", pattern).and_then(|a| a.object.pointer("/spec/zone_name").and_then(Value::as_str).map(str::to_owned)).unwrap_or_else(|| pattern.split('/').next().unwrap_or_default().trim_start_matches("*.").to_owned());
                let (_, list) = routes(cx, &zone).await?;
                if let Some(r) = list.iter().find(|r| r.get("pattern").and_then(Value::as_str) == Some(pattern)) {
                    out.push(kiln_cd::providers::object(GROUP, "Route", pattern, None, json!({ "pattern": pattern, "zone_name": zone, "script": r.get("script") })));
                }
            }
        }
        "CustomDomain" => {
            for (_, host) in names {
                if let Some(d) = domain(cx, host).await? {
                    let mut spec = json!({ "hostname": host, "service": d.get("service") });
                    if let (Some(z), Value::Object(m)) = (d.get("zone_name"), &mut spec) {
                        m.insert("zone_name".into(), z.clone());
                    }
                    out.push(kiln_cd::providers::object(GROUP, "CustomDomain", host, None, spec));
                }
            }
        }
        _ => {}
    }
    Ok(out)
}

pub async fn apply(cx: &Cx<'_>, desired: &Value) -> Result<Value> {
    match obj::kind(desired) {
        "Worker" => apply_worker(cx, desired).await,
        "Route" => apply_route(cx, desired).await,
        "CustomDomain" => apply_domain(cx, desired).await,
        other => bail!("cloudflare-workers doesn't apply {other}"),
    }
}

async fn apply_worker(cx: &Cx<'_>, desired: &Value) -> Result<Value> {
    let name = obj::name(desired);
    let (cf, account) = api(cx)?;
    let base = format!("/accounts/{account}/workers/scripts/{}", encode(name, false));
    let spec = desired.get("spec").cloned().unwrap_or(Value::Null);
    let all = scripts(cx).await?;
    let live = match all.get(name) {
        Some(s) => Some(cx.with_applied(worker(cx, name, s).await?)),
        None => None,
    };
    let tag = all.get(name).and_then(|s| s.get("migration_tag")).and_then(Value::as_str).map(str::to_owned);
    // What the upload's metadata says, and whether the Worker already has all of it.
    let upload_fields = ["bundle", "main_module", "compatibility_date", "compatibility_flags", "bindings", "migrations", "observability", "placement", "limits", "logpush", "tail_consumers"];
    let wanted: Map<String, Value> = upload_fields.iter().filter_map(|k| spec.get(*k).map(|v| ((*k).to_owned(), v.clone()))).collect();
    // A secret kiln put there that the app no longer has is an upload too (which deletes it).
    let names = |o: Option<&Value>| kiln_secrets(o).iter().filter_map(|b| b.get("name").and_then(Value::as_str).map(str::to_owned)).collect::<Vec<_>>();
    let now = names(Some(desired));
    let stale = names(cx.applied(GROUP, "Worker", "", name).as_ref().map(|r| &r.object)).iter().any(|n| !now.contains(n));
    let current = !stale && live.as_ref().is_some_and(|l| l.get("spec").is_some_and(|ls| kiln_cd::providers::aws_lambda::covers(&Value::Object(wanted.clone()), ls)));
    let tagged = live.as_ref().is_some_and(|l| obj::label(l, kiln_cd::engine::APP_LABEL) == obj::label(desired, kiln_cd::engine::APP_LABEL));
    let mut made = cx.applied(GROUP, "Worker", "", name).map(|r| r.made).unwrap_or(Value::Null);
    if !current || !tagged {
        let hex = spec.get("bundle").and_then(Value::as_str).and_then(|b| b.strip_prefix("sha256:")).context("spec.bundle isn't an artifact")?;
        let zip = crate::cd::artifacts::read(cx.files, hex).await?;
        let modules = crate::cd::artifacts::unzip(&zip, crate::cd::artifacts::LIMIT)?;
        let main = spec.get("main_module").and_then(Value::as_str).unwrap_or("index.js");
        if !modules.iter().any(|m| m.name == main) {
            bail!("the bundle has no {main} (it has {})", modules.iter().map(|m| m.name.as_str()).collect::<Vec<_>>().join(", "));
        }
        let mut metadata = Map::new();
        for k in ["main_module", "compatibility_date", "compatibility_flags", "bindings", "observability", "placement", "limits", "logpush", "tail_consumers"] {
            if let Some(v) = spec.get(k) {
                metadata.insert(k.into(), v.clone());
            }
        }
        metadata.insert("keep_bindings".into(), json!(["secret_text", "secret_key"]));
        let label = obj::label(desired, kiln_cd::engine::APP_LABEL).unwrap_or_default();
        metadata.insert("tags".into(), json!([format!("{TAG}{label}")]));
        let mut new_tag = tag.clone();
        if let Some(ms) = spec.get("migrations").and_then(Value::as_array)
            && let Some((steps, to)) = p::steps_after(ms, tag.as_deref()).map_err(|e| anyhow::anyhow!("{e}"))?
        {
            let mut m = json!({ "new_tag": to, "steps": steps });
            if let (Some(t), Value::Object(o)) = (&tag, &mut m) {
                o.insert("old_tag".into(), Value::String(t.clone()));
            }
            metadata.insert("migrations".into(), m);
            new_tag = Some(to);
        }
        // The app's secrets' values, only in what goes to Cloudflare.
        let metadata = super::resolve_secrets(cx, desired, Value::Object(metadata)).await?;
        let mut parts = vec![Part { name: "metadata".into(), filename: None, content_type: "application/json".into(), data: serde_json::to_vec(&metadata)? }];
        for m in modules {
            parts.push(Part { name: m.name.clone(), filename: Some(m.name.clone()), content_type: p::module_type(&m.name).into(), data: m.data });
        }
        cx.conn.budget().await;
        cf.upload(Method::Put, &base, &parts).await.with_context(|| format!("uploading Worker {name}"))?;
        // The app's secrets it no longer has: kept across the upload, so deleted.
        let now: Vec<Value> = kiln_secrets(Some(desired)).iter().filter_map(|b| b.get("name").cloned()).collect();
        for b in kiln_secrets(cx.applied(GROUP, "Worker", "", name).as_ref().map(|r| &r.object)) {
            let Some(gone) = b.get("name").and_then(Value::as_str).filter(|n| !now.iter().any(|x| x.as_str() == Some(n))) else { continue };
            cx.conn.budget().await;
            match cf.call(Method::Delete, &format!("{base}/secrets/{}", encode(gone, false)), None).await {
                Ok(_) => {}
                Err(e) if CfError::is_not_found(&e) => {}
                Err(e) => return Err(e.context(format!("deleting secret {gone} of Worker {name}"))),
            }
        }
        cx.conn.budget().await;
        let deployments = cf.call(Method::Get, &format!("{base}/deployments"), None).await?;
        let version = deployments.pointer("/deployments/0/versions/0/version_id").and_then(Value::as_str).map(str::to_owned);
        made = json!({ "version_id": version, "migration_tag": new_tag });
    }
    // Cron triggers and the workers.dev route, as the config says.
    if let Some(crons) = spec.get("crons").and_then(Value::as_array)
        && live.as_ref().and_then(|l| l.pointer("/spec/crons")) != Some(&Value::Array(crons.clone()))
    {
        let body = Value::Array(crons.iter().map(|c| json!({ "cron": c })).collect());
        cx.conn.budget().await;
        cf.call(Method::Put, &format!("{base}/schedules"), Some(&body)).await?;
    }
    if let Some(on) = spec.get("workers_dev").and_then(Value::as_bool)
        && live.as_ref().and_then(|l| l.pointer("/spec/workers_dev")).and_then(Value::as_bool) != Some(on)
    {
        cx.conn.budget().await;
        cf.call(Method::Post, &format!("{base}/subdomain"), Some(&json!({ "enabled": on }))).await?;
    }
    cx.record(GROUP, "Worker", "", name, Some(applied(desired, made))).await?;
    let all = scripts(cx).await?;
    worker(cx, name, all.get(name).with_context(|| format!("Worker {name} is gone"))?).await
}

async fn apply_route(cx: &Cx<'_>, desired: &Value) -> Result<Value> {
    let pattern = obj::name(desired);
    let zone = desired.pointer("/spec/zone_name").and_then(Value::as_str).context("the route has no zone")?;
    let script = desired.pointer("/spec/script").and_then(Value::as_str).context("the route names no Worker")?;
    let (id, list) = routes(cx, zone).await?;
    let (cf, _) = api(cx)?;
    let body = json!({ "pattern": pattern, "script": script });
    cx.conn.budget().await;
    match list.iter().find(|r| r.get("pattern").and_then(Value::as_str) == Some(pattern)).and_then(|r| r.get("id")).and_then(Value::as_str) {
        Some(rid) => cf.call(Method::Put, &format!("/zones/{id}/workers/routes/{rid}"), Some(&body)).await?,
        None => cf.call(Method::Post, &format!("/zones/{id}/workers/routes"), Some(&body)).await?,
    };
    cx.record(GROUP, "Route", "", pattern, Some(applied(desired, Value::Null))).await?;
    Ok(kiln_cd::providers::object(GROUP, "Route", pattern, None, json!({ "pattern": pattern, "zone_name": zone, "script": script })))
}

async fn apply_domain(cx: &Cx<'_>, desired: &Value) -> Result<Value> {
    let host = obj::name(desired);
    let service = desired.pointer("/spec/service").and_then(Value::as_str).context("the domain names no Worker")?;
    let (cf, account) = api(cx)?;
    let mut body = json!({ "hostname": host, "service": service, "environment": "production" });
    if let Some(z) = desired.pointer("/spec/zone_name").and_then(Value::as_str) {
        let zid = zone_id(cx, z).await?;
        if let Value::Object(m) = &mut body {
            m.insert("zone_id".into(), Value::String(zid));
        }
    }
    cx.conn.budget().await;
    cf.call(Method::Put, &format!("/accounts/{account}/workers/domains"), Some(&body)).await?;
    cx.record(GROUP, "CustomDomain", "", host, Some(applied(desired, Value::Null))).await?;
    let d = domain(cx, host).await?.with_context(|| format!("custom domain {host} is gone"))?;
    let mut spec = json!({ "hostname": host, "service": d.get("service") });
    if let (Some(z), Value::Object(m)) = (d.get("zone_name"), &mut spec) {
        m.insert("zone_name".into(), z.clone());
    }
    Ok(kiln_cd::providers::object(GROUP, "CustomDomain", host, None, spec))
}

pub async fn delete(cx: &Cx<'_>, key: &ResKey) -> Result<()> {
    let (cf, account) = api(cx)?;
    let name: &str = &key.name;
    let gone = |r: Result<Value>| match r {
        Ok(_) => Ok(()),
        Err(e) if CfError::is_not_found(&e) => Ok(()),
        Err(e) => Err(e),
    };
    match &*key.kind {
        "Worker" => {
            cx.conn.budget().await;
            gone(cf.call(Method::Delete, &format!("/accounts/{account}/workers/scripts/{}?force=true", encode(name, false)), None).await)
        }
        "Route" => {
            let zone = cx.applied(GROUP, "Route", "", name).and_then(|a| a.object.pointer("/spec/zone_name").and_then(Value::as_str).map(str::to_owned)).unwrap_or_else(|| name.split('/').next().unwrap_or_default().to_owned());
            let (id, list) = routes(cx, &zone).await?;
            let Some(rid) = list.iter().find(|r| r.get("pattern").and_then(Value::as_str) == Some(name)).and_then(|r| r.get("id")).and_then(Value::as_str).map(str::to_owned) else { return Ok(()) };
            cx.conn.budget().await;
            gone(cf.call(Method::Delete, &format!("/zones/{id}/workers/routes/{rid}"), None).await)
        }
        "CustomDomain" => {
            let Some(d) = domain(cx, name).await? else { return Ok(()) };
            let id = d.get("id").and_then(Value::as_str).unwrap_or_default().to_owned();
            cx.conn.budget().await;
            gone(cf.call(Method::Delete, &format!("/accounts/{account}/workers/domains/{id}"), None).await)
        }
        other => bail!("cloudflare-workers doesn't delete {other}"),
    }
}

/// What the account holds: its Workers (§8.2, §8.3).
pub async fn discover(cx: &Cx<'_>) -> Result<Value> {
    let all = scripts(cx).await?;
    let out: Vec<Value> = all
        .iter()
        .map(|(name, s)| json!({ "kind": "Worker", "name": name, "summary": format!("modified {}", s.get("modified_on").and_then(Value::as_str).unwrap_or("?")), "updated": s.get("modified_on") }))
        .collect();
    Ok(json!({ "resources": out }))
}

/// A Worker as it runs, as the definition that adopts it: its name, and the bundle CI builds.
pub async fn adopt(cx: &Cx<'_>, name: &str) -> Result<Value> {
    let all = scripts(cx).await?;
    if !all.contains_key(name) {
        bail!("no Worker {name} here");
    }
    Ok(json!({
        "definition": { "name": name, "bundle": "${{ artifacts.bundle }}" },
        "note": "kiln reads the Worker's settings, bindings and routes from the wrangler config in the app's source; point bundle at the zip CI builds (an artifact).",
    }))
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use kiln_cd::providers::{RenderInput, Target};

    use super::*;
    use crate::cd::targets::fake::{self, MemFiles, MemRecords};
    use crate::cd::targets::{Conn, Creds, key_of};

    const CONFIG: &str = r#"{
      "name": "edge", "main": "src/index.ts", "compatibility_date": "2026-09-01",
      "vars": { "MODE": "prod" },
      "durable_objects": { "bindings": [{ "name": "COUNTER", "class_name": "Counter" }] },
      "migrations": [{ "tag": "v1", "new_sqlite_classes": ["Counter"] }],
      "triggers": { "crons": ["*/5 * * * *"] },
      "routes": [{ "pattern": "api.example.com/*", "zone_name": "example.com" }, { "pattern": "edge.example.com", "custom_domain": true }]
    }"#;

    fn desired(config: &'static str, hex: &str) -> anyhow::Result<Vec<Value>> {
        let spec = json!({ "bundle": format!("sha256:{hex}") });
        let none = serde_json::Map::new();
        let read = move |p: &str| (p == "wrangler.jsonc").then(|| config.as_bytes().to_vec());
        let objs = p::render(&RenderInput { spec: &spec, target: &none, app: "edge", namespace: "", revision: "abc", read: &read }).map_err(|e| anyhow::anyhow!("{e}"))?;
        Ok(objs.into_iter().map(|o| fake::tracked(o, "default/edge-production")).collect())
    }

    /// A Worker's life on a faked Cloudflare: uploaded with its modules, bindings, migration and
    /// tag; its routes and domain after it; uploaded again only when its bundle or settings
    /// change; a version deployed outside kiln read as drift; then deleted.
    #[test]
    fn workers_upload_route_and_migrate() -> anyhow::Result<()> {
        let _one = fake::CLOUDFLARE.lock();
        crate::rt::block_on(async {
            let cf = Arc::new(fake::FakeCf::default());
            let base = fake::start_cf(cf.clone()).await?;
            crate::ci::integrations::override_endpoint("cloudflare", &base);
            let target = Target::parse("cloudflare-workers/cloudflare").ok_or_else(|| anyhow::anyhow!("target"))?;
            let def = target.def().ok_or_else(|| anyhow::anyhow!("provider"))?;
            let conn = Conn { settle_every: std::time::Duration::from_millis(5), gap: std::time::Duration::ZERO, ..Conn::new(target, def) };
            let (records, files) = (MemRecords::default(), MemFiles::default());
            let cx = Cx { conn: &conn, creds: Creds::Token { token: "cf-token".into(), config: json!({ "account_id": "acct1" }) }, records: &records, files: &files };

            let hex = files.add(&crate::cd::artifacts::tests::zip(&[("index.js", b"export default { fetch() { return new Response('hi') } }"), ("index.js.map", b"{}")]));
            let objs = desired(CONFIG, &hex)?;
            let live = cx.with_applied(apply(&cx, objs.first().ok_or_else(|| anyhow::anyhow!("worker"))?).await?);
            assert_eq!(live.pointer("/spec/bundle"), Some(&json!(format!("sha256:{hex}"))), "the bundle reads as kiln uploaded it");
            assert_eq!(obj::label(&live, kiln_cd::engine::APP_LABEL), Some(kiln_cd::engine::app_label("default/edge-production").as_str()));
            {
                let st = cf.state.lock();
                let up = st.uploads.last().cloned().unwrap_or_default();
                assert_eq!(up.pointer("/metadata/main_module"), Some(&json!("index.js")));
                assert_eq!(up.pointer("/metadata/migrations"), Some(&json!({ "new_tag": "v1", "steps": [{ "new_sqlite_classes": ["Counter"] }] })));
                assert_eq!(up.get("modules"), Some(&json!(["index.js", "index.js.map"])));
                assert_eq!(st.schedules.get("edge"), Some(&json!(["*/5 * * * *"])));
            }
            for o in objs.iter().skip(1) {
                apply(&cx, o).await?;
            }
            assert_eq!(list(&cx, "Route", &[(String::new(), "api.example.com/*".to_owned())]).await?.len(), 1);
            assert_eq!(list(&cx, "CustomDomain", &[(String::new(), "edge.example.com".to_owned())]).await?.len(), 1);

            // The same again uploads nothing.
            let w = objs.first().cloned().unwrap_or_default();
            apply(&cx, &w).await?;
            assert_eq!(cf.state.lock().uploads.len(), 1);

            // A new bundle: a new upload, and no migration (the Worker has v1).
            let hex2 = files.add(&crate::cd::artifacts::tests::zip(&[("index.js", b"export default { fetch() { return new Response('v2') } }")]));
            let w2 = desired(CONFIG, &hex2)?.first().cloned().unwrap_or_default();
            apply(&cx, &w2).await?;
            {
                let st = cf.state.lock();
                assert_eq!(st.uploads.len(), 2);
                assert_eq!(st.uploads.last().and_then(|u| u.pointer("/metadata/migrations")), None);
            }

            // The app's secrets (docs/deploy-api.md §3.5): secret_text bindings of an upload, their
            // text Cloudflare's alone; the Worker, its record and what views show hold kiln's
            // reference. The same again uploads nothing; one taken out of the app is deleted.
            files.1.lock().insert("kiln-secret:app/edge/production/API_KEY@1".into(), b"s3cret".to_vec());
            let mut objs = desired(CONFIG, &hex2)?;
            kiln_cd::apps::platform_secrets(&mut objs, "app/edge/production", &[("API_KEY".into(), 1)]).map_err(|e| anyhow::anyhow!("{e}"))?;
            let w4 = objs.first().cloned().unwrap_or_default();
            let live = apply(&cx, &w4).await?;
            assert_eq!(cf.state.lock().scripts.get("edge").and_then(|s| s.secrets.get("API_KEY").cloned()).as_deref(), Some("s3cret"), "the text is Cloudflare's");
            assert!(live.pointer("/spec/bindings").and_then(Value::as_array).is_some_and(|b| b.contains(&json!({ "type": "secret_text", "name": "API_KEY", "text": "kiln-secret:app/edge/production/API_KEY@1" }))), "{live}");
            assert!(!live.to_string().contains("s3cret") && !format!("{:?}", records.0.lock()).contains("s3cret"), "no value in what kiln shows or keeps");
            apply(&cx, &w4).await?;
            assert_eq!(cf.state.lock().uploads.len(), 3);
            apply(&cx, &w2).await?;
            {
                let st = cf.state.lock();
                assert_eq!(st.uploads.len(), 4);
                assert!(st.scripts.get("edge").is_some_and(|s| s.secrets.is_empty()), "the secret the app no longer has is gone");
            }
            let back = list(&cx, "Worker", &[(String::new(), "edge".to_owned())]).await?;
            assert!(back.first().and_then(|o| o.pointer("/spec/bundle")).is_some_and(|b| b == &json!(format!("sha256:{hex2}"))), "kiln's version still serves after the delete: {back:?}");

            // Someone deploys another version: drift.
            cf.state.lock().foreign_deploy("edge");
            let drifted = list(&cx, "Worker", &[(String::new(), "edge".to_owned())]).await?;
            assert!(drifted.first().and_then(|o| o.pointer("/spec/bundle")).and_then(Value::as_str).is_some_and(|b| b.starts_with("version:")));

            // A bundle without the entry module is refused before anything uploads.
            let hex3 = files.add(&crate::cd::artifacts::tests::zip(&[("main.js", b"x")]));
            let w3 = desired(CONFIG, &hex3)?.first().cloned().unwrap_or_default();
            assert!(apply(&cx, &w3).await.is_err_and(|e| format!("{e:#}").contains("no index.js")));

            for o in objs.iter().rev() {
                delete(&cx, &key_of(o)).await?;
            }
            assert!(list(&cx, "Worker", &[(String::new(), "edge".to_owned())]).await?.is_empty());
            assert!(list(&cx, "Route", &[(String::new(), "api.example.com/*".to_owned())]).await?.is_empty());
            Ok(())
        })
    }
}
