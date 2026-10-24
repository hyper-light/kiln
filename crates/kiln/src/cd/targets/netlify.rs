//! `netlify`'s I/O half (docs/cd-targets.md §3): Netlify's API. The pure half,
//! `kiln_cd::providers::netlify`, renders the objects.
//!
//! A release reads the built site, makes a deploy naming each file by its SHA-1, uploads the files
//! Netlify asks for, and waits until the deploy is ready (published, as it isn't a draft). kiln
//! keeps the deploys it made by site, so a rollback to one of them restores it.

use anyhow::{Result, bail};
use kiln_cd::obj::{self, ResKey};
use kiln_cd::providers::netlify::{self as p, GROUP};
use serde_json::{Map, Value, json};

use super::{Cx, applied, until};
use crate::ci::integrations::aws::encode;
use crate::ci::integrations::hosting::Api;
use crate::ci::integrations::vercel::sha1_hex;
use crate::http::client::Method;

/// The longest a deploy takes to be ready.
const READY: std::time::Duration = std::time::Duration::from_secs(1800);
/// How many of its deploys kiln keeps to roll back to.
const KEEP: usize = 20;

fn api<'a>(cx: &'a Cx<'_>) -> Result<Api<'a>> {
    Ok(Api { kind: "netlify", token: cx.token()?.0 })
}

async fn call(cx: &Cx<'_>, method: Method, path: &str, body: Option<&Value>) -> Result<Value> {
    cx.conn.budget().await;
    api(cx)?.call(method, &format!("/api/v1{path}"), body, &[]).await
}

/// A site by its name.
async fn site(cx: &Cx<'_>, name: &str) -> Result<Option<Value>> {
    let found = call(cx, Method::Get, &format!("/sites?name={}&filter=all", encode(name, false)), None).await?;
    Ok(found.as_array().into_iter().flatten().find(|s| s.get("name").and_then(Value::as_str) == Some(name)).cloned())
}

async fn newest(cx: &Cx<'_>, id: &str) -> Result<Option<Value>> {
    Ok(call(cx, Method::Get, &format!("/sites/{id}/deploys?per_page=1"), None).await?.get(0).cloned())
}

async fn live(cx: &Cx<'_>, name: &str) -> Result<Option<Value>> {
    let Some(s) = site(cx, name).await? else { return Ok(None) };
    let n = newest(cx, s.get("id").and_then(Value::as_str).unwrap_or_default()).await?;
    let rec = cx.applied(GROUP, "Site", "", name);
    let published = s.pointer("/published_deploy/id").and_then(Value::as_str);
    let runs = rec.as_ref().is_some_and(|r| published.is_some() && r.made.get("deploy").and_then(Value::as_str) == published);
    Ok(p::live_site(&s, n.as_ref(), rec.as_ref().map(|r| &r.object), runs))
}

pub async fn list(cx: &Cx<'_>, kind: &str, names: &[(String, String)]) -> Result<Vec<Value>> {
    let mut out = Vec::new();
    for (_, name) in names {
        if kind == "Site" {
            out.extend(live(cx, name).await?);
        }
    }
    Ok(out)
}

/// What the account holds: its sites (§8.2, §8.3).
pub async fn discover(cx: &Cx<'_>) -> Result<Value> {
    let sites = call(cx, Method::Get, "/sites?filter=all", None).await?;
    let out: Vec<Value> = sites.as_array().into_iter().flatten().map(|s| json!({ "kind": "Site", "name": s.get("name"), "summary": s.get("ssl_url").or_else(|| s.get("url")), "updated": s.get("updated_at") })).collect();
    Ok(json!({ "resources": out }))
}

/// A site as it runs now, as the definition that adopts it (§8.2 step 2).
pub async fn adopt(cx: &Cx<'_>, name: &str) -> Result<Value> {
    if site(cx, name).await?.is_none() {
        bail!("no site {name} here");
    }
    Ok(json!({ "definition": { "site": name, "dir": "${{ release.artifacts.site.digest }}" }, "note": "Netlify doesn't say where a deploy's files came from: point dir at the site CI builds" }))
}

/// Waits for a deploy to be ready: why, if it erred.
async fn ready(cx: &Cx<'_>, id: &str, what: &str) -> Result<()> {
    let d = until(cx.conn, READY, what, || async {
        let d = api(cx)?.call(Method::Get, &format!("/api/v1/deploys/{id}"), None, &[]).await?;
        Ok(matches!(d.get("state").and_then(Value::as_str), Some("ready" | "error")).then_some(d))
    })
    .await?;
    if d.get("state").and_then(Value::as_str) == Some("error") {
        bail!("{what}: the deploy failed: {}", d.get("error_message").and_then(Value::as_str).unwrap_or("Netlify says no more"));
    }
    Ok(())
}

/// Applies a Site: made if it isn't there, then its site deployed unless it's published already;
/// a deploy kiln made of it before is restored.
pub async fn apply(cx: &Cx<'_>, desired: &Value) -> Result<Value> {
    let name = obj::name(desired);
    let s = match site(cx, name).await? {
        Some(s) => s,
        None => call(cx, Method::Post, "/sites", Some(&json!({ "name": name }))).await?,
    };
    let id = s.get("id").and_then(Value::as_str).ok_or_else(|| anyhow::anyhow!("Netlify answered no site"))?.to_owned();
    let artifact = desired.pointer("/spec/dir/artifact").and_then(Value::as_str).ok_or_else(|| anyhow::anyhow!("site {name} has no dir"))?.to_owned();
    let prev = cx.applied(GROUP, "Site", "", name).map(|a| a.made).unwrap_or(Value::Null);
    let mut made: Map<String, Value> = prev.get("deploys").and_then(Value::as_object).cloned().unwrap_or_default();
    let published = s.pointer("/published_deploy/id").and_then(Value::as_str).map(str::to_owned);
    let what = format!("deploying site {name}");
    let deploy = if prev.get("artifact").and_then(Value::as_str) == Some(artifact.as_str()) && published.is_some() && published.as_deref() == prev.get("deploy").and_then(Value::as_str) {
        published.unwrap_or_default()
    } else if let Some(before) = made.get(&artifact).and_then(Value::as_str).map(str::to_owned) {
        // A deploy kiln made of this site before: restored, nothing uploaded.
        call(cx, Method::Post, &format!("/sites/{id}/deploys/{before}/restore"), None).await?;
        before
    } else {
        let files = super::aws_static::read_site(cx, &artifact).await?;
        let digests: Map<String, Value> = files.iter().map(|(path, data)| (format!("/{path}"), json!(sha1_hex(data)))).collect();
        let d = call(cx, Method::Post, &format!("/sites/{id}/deploys"), Some(&json!({ "files": digests, "draft": false }))).await?;
        let did = d.get("id").and_then(Value::as_str).ok_or_else(|| anyhow::anyhow!("{what}: Netlify answered no deploy"))?.to_owned();
        let required: Vec<String> = d.get("required").and_then(Value::as_array).into_iter().flatten().filter_map(Value::as_str).map(str::to_owned).collect();
        let mut sent = std::collections::BTreeSet::new();
        for (path, data) in &files {
            let sha = sha1_hex(data);
            if required.contains(&sha) && sent.insert(sha) {
                let p = path.split('/').map(|s| encode(s, false)).collect::<Vec<_>>().join("/");
                cx.conn.budget().await;
                api(cx)?.upload(Method::Put, &format!("/api/v1/deploys/{did}/files/{p}"), "application/octet-stream", data.clone(), true).await?;
            }
        }
        cx.record(GROUP, "Site", "", name, Some(applied(desired, json!({ "artifact": artifact, "deploy": did, "deploys": made })))).await?;
        ready(cx, &did, &what).await?;
        did
    };
    made.insert(artifact.clone(), json!(deploy));
    while made.len() > KEEP {
        let Some(k) = made.keys().next().cloned() else { break };
        made.remove(&k);
    }
    cx.record(GROUP, "Site", "", name, Some(applied(desired, json!({ "artifact": artifact, "deploy": deploy, "deploys": made })))).await?;
    live(cx, name).await?.ok_or_else(|| anyhow::anyhow!("site {name} is gone"))
}

pub async fn delete(cx: &Cx<'_>, key: &ResKey) -> Result<()> {
    if let Some(s) = site(cx, &key.name).await? {
        call(cx, Method::Delete, &format!("/sites/{}", s.get("id").and_then(Value::as_str).unwrap_or_default()), None).await?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::io::Write as _;
    use std::sync::Arc;

    use kiln_cd::model::Health;

    use super::*;
    use crate::cd::targets::fake::{MemFiles, MemRecords};
    use crate::cd::targets::fake_gcp::synced;
    use crate::cd::targets::fake_hosting::{self, FakeHosting};
    use crate::cd::targets::fake_query::released_to;
    use crate::cd::targets::{Conn, Creds, key_of};

    fn zip_of(files: &[(&str, &str)]) -> anyhow::Result<Vec<u8>> {
        let mut w = zip::ZipWriter::new(std::io::Cursor::new(Vec::new()));
        for (name, text) in files {
            w.start_file(*name, zip::write::SimpleFileOptions::default())?;
            w.write_all(text.as_bytes())?;
        }
        Ok(w.finish()?.into_inner())
    }

    /// A site on a faked Netlify, release by release: the first makes the site and deploys its
    /// files, each uploaded once, and it's published once ready; the same release again deploys
    /// nothing; the next uploads only the files that changed; the release before, rolled back
    /// to, restores its deploy with nothing uploaded; one whose deploy fails reads as degraded,
    /// the deploy before published; then the site goes.
    #[test]
    fn sites_publish_each_releases_files() -> anyhow::Result<()> {
        let _one = fake_hosting::HOSTING.lock();
        crate::rt::block_on(async {
            let fake = Arc::new(FakeHosting::default());
            fake_hosting::start(fake.clone()).await?;
            let (target, place) = fake_hosting::target("netlify")?;
            let def = target.def().ok_or_else(|| anyhow::anyhow!("provider"))?;
            let conn = Conn::quick(target, def);
            let (records, files) = (MemRecords::default(), MemFiles::default());
            let cx = Cx { conn: &conn, creds: Creds::Token { token: fake_hosting::TOKEN.into(), config: json!({}) }, records: &records, files: &files };
            let spec = json!({ "site": "shop-site", "dir": "${{ release.artifacts.site.digest }}" });
            let release = |version: &str, site: &[(&str, &str)]| -> anyhow::Result<Value> {
                let digest = format!("sha256:{}", files.add(&zip_of(site)?));
                released_to("netlify", &place, spec.clone(), version, &[], &[("site", &digest)])?.into_iter().next().ok_or_else(|| anyhow::anyhow!("no site"))
            };
            let names = [(String::new(), "shop-site".to_owned())];
            let h = |o: &Value| kiln_cd::providers::health(o).map(|s| s.health);
            let uploads = || fake.state.lock().calls.iter().filter(|c| c.starts_with("UploadFile")).count();
            let last_call = || fake.state.lock().calls.last().cloned().unwrap_or_default();
            let v1 = [("index.html", "<h1>shop 1</h1>"), ("assets/app.js", "app()")];
            let v2 = [("index.html", "<h1>shop 2</h1>"), ("assets/app.js", "app()")];

            let s1 = release("1.0.0", &v1)?;
            let up = apply(&cx, &s1).await?;
            assert_eq!(h(&up), Some(Health::Healthy), "{up}");
            assert!(synced(&s1, &cx.with_applied(up.clone())), "in sync: {up}");
            assert_eq!(uploads(), 2);
            apply(&cx, &release("1.0.0", &v1)?).await?;
            assert_eq!(uploads(), 2, "the same release again deploys nothing");

            let s2 = release("1.1.0", &v2)?;
            apply(&cx, &s2).await?;
            assert_eq!(uploads(), 3, "only the file that changed went up");

            // Rolled back to the release before: its deploy restored, nothing uploaded.
            let back = apply(&cx, &s1).await?;
            assert!(last_call().starts_with("Restore shop-site deploy-"), "{}", last_call());
            assert_eq!(uploads(), 3);
            assert!(synced(&s1, &cx.with_applied(back.clone())) && h(&back) == Some(Health::Healthy), "{back}");

            // One whose deploy fails: it fails, read as degraded, the deploy before published.
            let failed = apply(&cx, &release("1.2.0", &[("index.html", "broken")])?).await.err().map(|e| format!("{e:#}")).unwrap_or_default();
            assert!(failed.contains("deploy failed"), "{failed}");
            let down = list(&cx, "Site", &names).await?.into_iter().next().unwrap_or_default();
            assert_eq!(h(&down), Some(Health::Degraded), "{down}");

            let found = discover(&cx).await?;
            assert_eq!(found.pointer("/resources/0/name"), Some(&json!("shop-site")));
            delete(&cx, &key_of(&s1)).await?;
            assert!(list(&cx, "Site", &names).await?.is_empty(), "deleted");
            Ok(())
        })
    }
}
