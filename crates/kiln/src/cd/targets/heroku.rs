//! `heroku`'s I/O half (docs/cd-targets.md §3): Heroku's Platform API. The pure half,
//! `kiln_cd::providers::heroku`, renders the objects.
//!
//! A release sets the config vars kiln sets, then sends its source to a place Heroku's sources
//! give (`POST /sources`), builds it, and waits for the build, its release and the dynos on it.
//! kiln keeps the releases it made, by what they ran, so a rollback to one of them is Heroku's
//! rollback to that release.

use anyhow::{Result, bail};
use kiln_cd::obj::{self, ResKey};
use kiln_cd::providers::heroku::{self as p, GROUP};
use serde_json::{Map, Value, json};

use super::{Cx, applied, until};
use crate::ci::integrations::aws::encode;
use crate::ci::integrations::hosting::{Api, HostingError};
use crate::http::client::Method;

/// The longest a build and its dynos take.
const SETTLE: std::time::Duration = std::time::Duration::from_secs(1800);
/// How many of its releases kiln keeps to roll back to.
const KEEP: usize = 20;
/// Newest first, as Heroku's ranges say it.
const NEWEST: &str = "version ..; order=desc, max=20;";

fn api<'a>(cx: &'a Cx<'_>) -> Result<Api<'a>> {
    Ok(Api { kind: "heroku", token: cx.token()?.0 })
}

async fn call(cx: &Cx<'_>, method: Method, path: &str, body: Option<&Value>, headers: &[(&str, &str)]) -> Result<Value> {
    cx.conn.budget().await;
    api(cx)?.call(method, path, body, headers).await
}

fn missing<T>(r: Result<T>) -> Result<Option<T>> {
    match r {
        Ok(v) => Ok(Some(v)),
        Err(e) if HostingError::is_not_found(&e) => Ok(None),
        Err(e) => Err(e),
    }
}

fn app_path(name: &str) -> String {
    format!("/apps/{}", encode(name, false))
}

async fn releases(cx: &Cx<'_>, name: &str) -> Result<Vec<Value>> {
    Ok(call(cx, Method::Get, &format!("{}/releases", app_path(name)), None, &[("Range", NEWEST)]).await?.as_array().cloned().unwrap_or_default())
}

async fn live(cx: &Cx<'_>, name: &str) -> Result<Option<Value>> {
    let Some(app) = missing(call(cx, Method::Get, &app_path(name), None, &[]).await)? else { return Ok(None) };
    let rs = releases(cx, name).await?;
    let current = rs.iter().find(|r| r.get("current").and_then(Value::as_bool) == Some(true)).cloned();
    let config = call(cx, Method::Get, &format!("{}/config-vars", app_path(name)), None, &[]).await?;
    let dynos = call(cx, Method::Get, &format!("{}/dynos", app_path(name)), None, &[]).await?.as_array().cloned().unwrap_or_default();
    let builds = call(cx, Method::Get, &format!("{}/builds", app_path(name)), None, &[("Range", "created_at ..; order=desc, max=1;")]).await?;
    let rec = cx.applied(GROUP, "App", "", name);
    let runs = rec.as_ref().is_some_and(|r| current.as_ref().and_then(|c| c.get("id")) == r.made.get("release_id") && r.made.get("release_id").is_some_and(|v| !v.is_null()));
    Ok(p::live_app(&app, current.as_ref(), &config, &dynos, builds.get(0), rec.as_ref().map(|r| &r.object), runs))
}

pub async fn list(cx: &Cx<'_>, kind: &str, names: &[(String, String)]) -> Result<Vec<Value>> {
    let mut out = Vec::new();
    for (_, name) in names {
        if kind == "App" {
            out.extend(live(cx, name).await?);
        }
    }
    Ok(out)
}

/// What the account holds: its apps (§8.2, §8.3).
pub async fn discover(cx: &Cx<'_>) -> Result<Value> {
    let apps = call(cx, Method::Get, "/apps", None, &[]).await?;
    let out: Vec<Value> = apps.as_array().into_iter().flatten().map(|a| json!({ "kind": "App", "name": a.get("name"), "summary": a.get("web_url"), "updated": a.get("updated_at") })).collect();
    Ok(json!({ "resources": out }))
}

/// An app as it runs now, as the definition that adopts it (§8.2 step 2).
pub async fn adopt(cx: &Cx<'_>, name: &str) -> Result<Value> {
    if missing(call(cx, Method::Get, &app_path(name), None, &[]).await)?.is_none() {
        bail!("no app {name} here");
    }
    Ok(json!({ "definition": { "app": name, "source": "${{ release.artifacts.source.digest }}" }, "note": "Heroku doesn't say where a build's source came from: point source at the .tar.gz CI makes" }))
}

/// Waits for a release to settle and its dynos to start on it: why, if they didn't.
async fn settled(cx: &Cx<'_>, name: &str, release: &str, what: &str) -> Result<()> {
    let r = until(cx.conn, SETTLE, what, || async {
        let r = api(cx)?.call(Method::Get, &format!("{}/releases/{release}", app_path(name)), None, &[]).await?;
        Ok((r.get("status").and_then(Value::as_str) != Some("pending")).then_some(r))
    })
    .await?;
    if r.get("status").and_then(Value::as_str) == Some("failed") {
        bail!("{what}: its release failed");
    }
    let dynos = until(cx.conn, SETTLE, what, || async {
        let d = api(cx)?.call(Method::Get, &format!("{}/dynos", app_path(name)), None, &[]).await?;
        let list = d.as_array().cloned().unwrap_or_default();
        Ok((!list.iter().any(|x| x.get("state").and_then(Value::as_str) == Some("starting"))).then_some(list))
    })
    .await?;
    let crashed = dynos.iter().filter(|d| d.get("state").and_then(Value::as_str) == Some("crashed")).count();
    if crashed > 0 {
        bail!("{what}: {crashed} of its {} dynos crashed", dynos.len());
    }
    Ok(())
}

/// Builds the source CI packed (by its SHA-256, in hex): the release the build made.
async fn build(cx: &Cx<'_>, name: &str, hex: &str, version: &str, what: &str) -> Result<String> {
    let src = call(cx, Method::Post, "/sources", None, &[]).await?;
    let (get_url, put_url) = (src.pointer("/source_blob/get_url").and_then(Value::as_str), src.pointer("/source_blob/put_url").and_then(Value::as_str));
    let (Some(get_url), Some(put_url)) = (get_url, put_url) else { bail!("{what}: Heroku gave no place for the source") };
    let (rx, _) = cx.files.open(hex).await?;
    let mut data = Vec::new();
    while let Ok(chunk) = rx.recv().await {
        data.extend_from_slice(&chunk);
    }
    cx.conn.budget().await;
    // The place is signed for no content type.
    api(cx)?.upload(Method::Put, put_url, "", data, false).await?;
    let body = json!({ "source_blob": { "url": get_url, "version": version, "checksum": format!("SHA256:{hex}") } });
    let b = call(cx, Method::Post, &format!("{}/builds", app_path(name)), Some(&body), &[]).await?;
    let id = b.get("id").and_then(Value::as_str).ok_or_else(|| anyhow::anyhow!("{what}: Heroku answered no build"))?.to_owned();
    let b = until(cx.conn, SETTLE, what, || async {
        let b = api(cx)?.call(Method::Get, &format!("{}/builds/{id}", app_path(name)), None, &[]).await?;
        Ok((b.get("status").and_then(Value::as_str) != Some("pending")).then_some(b))
    })
    .await?;
    match (b.get("status").and_then(Value::as_str), b.pointer("/release/id").and_then(Value::as_str)) {
        (Some("succeeded"), Some(r)) => Ok(r.to_owned()),
        _ => bail!("{what}: its build failed"),
    }
}

/// Applies an App: made if it isn't there, kiln's config vars set, then its release built
/// unless it runs already; a release kiln made before is rolled back to.
pub async fn apply(cx: &Cx<'_>, desired: &Value) -> Result<Value> {
    let name = obj::name(desired);
    let spec = desired.get("spec").cloned().unwrap_or(Value::Null);
    let what = format!("releasing {name}");
    if missing(call(cx, Method::Get, &app_path(name), None, &[]).await)?.is_none() {
        let mut body = json!({ "name": name });
        for k in ["stack", "region"] {
            if let (Some(v), Value::Object(m)) = (spec.get(k), &mut body) {
                m.insert(k.into(), v.clone());
            }
        }
        call(cx, Method::Post, "/apps", Some(&body), &[]).await?;
    }
    if let Some(Value::Object(want)) = spec.get("config") {
        let have = call(cx, Method::Get, &format!("{}/config-vars", app_path(name)), None, &[]).await?;
        let changed: Map<String, Value> = want.iter().filter(|(k, v)| have.get(*k) != Some(*v)).map(|(k, v)| (k.clone(), v.clone())).collect();
        if !changed.is_empty() {
            call(cx, Method::Patch, &format!("{}/config-vars", app_path(name)), Some(&Value::Object(changed)), &[]).await?;
        }
    }
    let want = kiln_cd::obj::hash_value(spec.get("source").unwrap_or(&Value::Null)).to_string();
    let prev = cx.applied(GROUP, "App", "", name).map(|a| a.made).unwrap_or(Value::Null);
    let mut made: Map<String, Value> = prev.get("releases").and_then(Value::as_object).cloned().unwrap_or_default();
    let current = releases(cx, name).await?.into_iter().find(|r| r.get("current").and_then(Value::as_bool) == Some(true)).and_then(|r| r.get("id").and_then(Value::as_str).map(str::to_owned));
    let release = if prev.get("release").and_then(Value::as_str) == Some(want.as_str()) && current.is_some() && current.as_deref() == prev.get("release_id").and_then(Value::as_str) {
        current.unwrap_or_default()
    } else {
        let r = match made.get(&want).and_then(Value::as_str) {
            // A release kiln made of this source before: Heroku rolls back to it.
            Some(before) => call(cx, Method::Post, &format!("{}/releases", app_path(name)), Some(&json!({ "release": before })), &[]).await?.get("id").and_then(Value::as_str).map(str::to_owned).ok_or_else(|| anyhow::anyhow!("{what}: Heroku answered no release"))?,
            None => {
                let hex = spec.pointer("/source/artifact").and_then(Value::as_str).unwrap_or_default().trim_start_matches("sha256:").to_owned();
                let version = hex.get(..12).unwrap_or(&hex).to_owned();
                build(cx, name, &hex, &version, &what).await?
            }
        };
        cx.record(GROUP, "App", "", name, Some(applied(desired, json!({ "release": want, "release_id": r, "releases": made })))).await?;
        settled(cx, name, &r, &what).await?;
        r
    };
    made.insert(want.clone(), json!(release));
    while made.len() > KEEP {
        let Some(k) = made.keys().next().cloned() else { break };
        made.remove(&k);
    }
    cx.record(GROUP, "App", "", name, Some(applied(desired, json!({ "release": want, "release_id": release, "releases": made })))).await?;
    live(cx, name).await?.ok_or_else(|| anyhow::anyhow!("app {name} is gone"))
}

pub async fn delete(cx: &Cx<'_>, key: &ResKey) -> Result<()> {
    missing(call(cx, Method::Delete, &app_path(&key.name), None, &[]).await)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use kiln_cd::model::Health;

    use super::*;
    use crate::cd::targets::fake::{MemFiles, MemRecords};
    use crate::cd::targets::fake_gcp::synced;
    use crate::cd::targets::fake_hosting::{self, FakeHosting};
    use crate::cd::targets::fake_query::released_to;
    use crate::cd::targets::{Conn, Creds, key_of};

    /// An app on a faked Heroku, release by release: the first makes the app, sets its config vars,
    /// sends its source to a place Heroku signed and builds it into a release its dynos run; the
    /// same release again changes nothing; the next builds its own; the release before, rolled
    /// back to, is Heroku's rollback to its release, built from nothing; one whose dynos crash
    /// fails and reads as degraded, and one that doesn't build fails too; then the app goes.
    #[test]
    fn apps_build_each_releases_source() -> anyhow::Result<()> {
        let _one = fake_hosting::HOSTING.lock();
        crate::rt::block_on(async {
            let fake = Arc::new(FakeHosting::default());
            fake_hosting::start(fake.clone()).await?;
            let (target, place) = fake_hosting::target("heroku")?;
            let def = target.def().ok_or_else(|| anyhow::anyhow!("provider"))?;
            let conn = Conn::quick(target, def);
            let (records, files) = (MemRecords::default(), MemFiles::default());
            let cx = Cx { conn: &conn, creds: Creds::Token { token: fake_hosting::TOKEN.into(), config: json!({}) }, records: &records, files: &files };
            let spec = json!({ "app": "shop-web", "source": "${{ release.artifacts.source.digest }}", "config": { "MODE": "${{ env.name }}" } });
            let release = |version: &str, src: &str| -> anyhow::Result<Value> {
                let digest = format!("sha256:{}", files.add(src.as_bytes()));
                released_to("heroku", &place, spec.clone(), version, &[], &[("source", &digest)])?.into_iter().next().ok_or_else(|| anyhow::anyhow!("no app"))
            };
            let names = [(String::new(), "shop-web".to_owned())];
            let h = |o: &Value| kiln_cd::providers::health(o).map(|s| s.health);
            let count = |prefix: &str| fake.state.lock().calls.iter().filter(|c| c.starts_with(prefix)).count();
            let last_call = || fake.state.lock().calls.last().cloned().unwrap_or_default();

            let a1 = release("1.0.0", "tarball of shop-web 1.0.0")?;
            let up = apply(&cx, &a1).await?;
            assert_eq!(h(&up), Some(Health::Healthy), "{up}");
            assert!(synced(&a1, &cx.with_applied(up.clone())), "in sync: {up}");
            assert_eq!(fake.state.lock().calls, ["CreateApp shop-web", "SetConfig shop-web", "UploadSource", "Build shop-web"]);
            apply(&cx, &release("1.0.0", "tarball of shop-web 1.0.0")?).await?;
            assert_eq!(fake.state.lock().calls.len(), 4, "the same release again changes nothing");

            let a2 = release("1.1.0", "tarball of shop-web 1.1.0")?;
            let up2 = apply(&cx, &a2).await?;
            assert_eq!((h(&up2), count("Build")), (Some(Health::Healthy), 2));

            // Rolled back to the release before: Heroku's rollback to its release.
            let back = apply(&cx, &a1).await?;
            assert!(last_call().starts_with("Rollback shop-web rel-"), "{}", last_call());
            assert_eq!(count("Build"), 2, "nothing built for a rollback");
            assert!(synced(&a1, &cx.with_applied(back.clone())) && h(&back) == Some(Health::Healthy), "{back}");

            // One whose dynos crash: it fails, and reads as degraded; going back is a rollback.
            let failed = apply(&cx, &release("1.2.0", "a broken tarball")?).await.err().map(|e| format!("{e:#}")).unwrap_or_default();
            assert!(failed.contains("crashed"), "{failed}");
            assert_eq!(h(&list(&cx, "App", &names).await?.into_iter().next().unwrap_or_default()), Some(Health::Degraded));
            let again = apply(&cx, &a2).await?;
            assert!(last_call().starts_with("Rollback shop-web rel-") && h(&again) == Some(Health::Healthy), "{again}");

            // One that doesn't build: it fails, and the newer failed build reads as degraded.
            let failed = apply(&cx, &release("1.3.0", "an unbuildable tarball")?).await.err().map(|e| format!("{e:#}")).unwrap_or_default();
            assert!(failed.contains("build failed"), "{failed}");
            assert_eq!(h(&list(&cx, "App", &names).await?.into_iter().next().unwrap_or_default()), Some(Health::Degraded));

            let found = discover(&cx).await?;
            assert_eq!(found.pointer("/resources/0/name"), Some(&json!("shop-web")));
            delete(&cx, &key_of(&a1)).await?;
            assert!(list(&cx, "App", &names).await?.is_empty(), "deleted");
            Ok(())
        })
    }
}
