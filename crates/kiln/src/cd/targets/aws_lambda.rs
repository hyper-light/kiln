//! `aws-lambda`'s I/O half (docs/cd-targets.md §3): Lambda's REST-JSON API. The pure half,
//! `kiln_cd::providers::aws_lambda`, reads its answers into objects and builds its requests.
//!
//! A function's apply makes Lambda's calls in its order: create the function, or update its code
//! and then its configuration (each waits for the one before to settle, as Lambda requires), then
//! publish a version when anything changed. The alias then moves to that version; the URL
//! follows the alias.

use std::time::Duration;

use anyhow::{Result, bail};
use kiln_cd::obj::{self, ResKey};
use kiln_cd::providers::aws_lambda::{self as p, CodeSource, GROUP, PUBLISHED};
use serde_json::{Value, json};

use super::{Cx, applied, until};
use crate::ci::integrations::aws::{AwsError, encode};
use crate::http::client::Method;

const FUNCTIONS: &str = "/2015-03-31/functions";
const URLS: &str = "/2021-10-31/functions";
/// The longest a function takes to settle after a change.
const SETTLE: Duration = Duration::from_secs(600);

fn function_path(name: &str) -> String {
    format!("{FUNCTIONS}/{}", encode(name, false))
}

/// GetFunction's answer, or none.
async fn get_function(cx: &Cx<'_>, name: &str) -> Result<Option<Value>> {
    cx.conn.budget().await;
    match cx.aws()?.rest("lambda", Method::Get, &function_path(name), &[], None).await {
        Ok(v) => Ok(Some(v)),
        Err(e) if AwsError::is_not_found(&e) => Ok(None),
        Err(e) => Err(e),
    }
}

/// The function as an object: code reads as what kiln applied while Lambda still holds it.
async fn function(cx: &Cx<'_>, name: &str) -> Result<Option<Value>> {
    let Some(answer) = get_function(cx, name).await? else { return Ok(None) };
    let rec = cx.applied(GROUP, "Function", "", name);
    let code = rec.as_ref().and_then(|a| Some((a.object.pointer("/spec/Code")?, a.made.get("CodeSha256")?.as_str()?)));
    Ok(p::live_function(&answer, code))
}

/// The version an alias at `$PUBLISHED` points to: the one kiln published with the function's
/// last change; for a function kiln never changed, its newest.
async fn published(cx: &Cx<'_>, function: &str) -> Result<Option<String>> {
    if let Some(v) = cx.applied(GROUP, "Function", "", function).and_then(|a| a.made.get("Version").and_then(Value::as_str).map(str::to_owned)) {
        return Ok(Some(v));
    }
    let mut newest: Option<u64> = None;
    let mut marker: Option<String> = None;
    loop {
        let mut q = vec![("MaxItems".to_owned(), "50".to_owned())];
        if let Some(m) = &marker {
            q.push(("Marker".to_owned(), m.clone()));
        }
        cx.conn.budget().await;
        let page = match cx.aws()?.rest("lambda", Method::Get, &format!("{}/versions", function_path(function)), &q, None).await {
            Ok(v) => v,
            Err(e) if AwsError::is_not_found(&e) => return Ok(None),
            Err(e) => return Err(e),
        };
        for v in page.get("Versions").and_then(Value::as_array).into_iter().flatten() {
            if let Some(n) = v.get("Version").and_then(Value::as_str).and_then(|s| s.parse::<u64>().ok()) {
                newest = newest.max(Some(n));
            }
        }
        marker = page.get("NextMarker").and_then(Value::as_str).filter(|m| !m.is_empty()).map(str::to_owned);
        if marker.is_none() {
            return Ok(newest.map(|n| n.to_string()));
        }
    }
}

async fn alias(cx: &Cx<'_>, function: &str, name: &str) -> Result<Option<Value>> {
    cx.conn.budget().await;
    let answer = match cx.aws()?.rest("lambda", Method::Get, &format!("{}/aliases/{}", function_path(function), encode(name, false)), &[], None).await {
        Ok(v) => v,
        Err(e) if AwsError::is_not_found(&e) => return Ok(None),
        Err(e) => return Err(e),
    };
    let newest = published(cx, function).await?;
    Ok(p::live_alias(function, &answer, newest.as_deref()))
}

async fn url(cx: &Cx<'_>, function: &str, qualifier: &str) -> Result<Option<Value>> {
    cx.conn.budget().await;
    let q = [("Qualifier".to_owned(), qualifier.to_owned())];
    match cx.aws()?.rest("lambda", Method::Get, &format!("{URLS}/{}/url", encode(function, false)), &q, None).await {
        Ok(v) => Ok(p::live_url(function, qualifier, &v)),
        Err(e) if AwsError::is_not_found(&e) => Ok(None),
        Err(e) => Err(e),
    }
}

/// `function:qualifier`, as aliases and URLs are named.
fn split(name: &str) -> Result<(&str, &str)> {
    name.split_once(':').ok_or_else(|| anyhow::anyhow!("`{name}` isn't function:alias"))
}

pub async fn list(cx: &Cx<'_>, kind: &str, names: &[(String, String)]) -> Result<Vec<Value>> {
    let mut out = Vec::new();
    for (_, name) in names {
        let o = match kind {
            "Function" => function(cx, name).await?,
            "Alias" => {
                let (f, a) = split(name)?;
                alias(cx, f, a).await?
            }
            "FunctionUrl" => {
                let (f, q) = split(name)?;
                url(cx, f, q).await?
            }
            _ => None,
        };
        out.extend(o);
    }
    Ok(out)
}

/// What the account holds in the region: its functions (§8.2, §8.3).
pub async fn discover(cx: &Cx<'_>) -> Result<Value> {
    let mut out = Vec::new();
    let mut marker: Option<String> = None;
    loop {
        let mut q = vec![("MaxItems".to_owned(), "50".to_owned())];
        if let Some(m) = &marker {
            q.push(("Marker".to_owned(), m.clone()));
        }
        cx.conn.budget().await;
        let page = cx.aws()?.rest("lambda", Method::Get, &format!("{FUNCTIONS}/"), &q, None).await?;
        for f in page.get("Functions").and_then(Value::as_array).into_iter().flatten() {
            let s = |k: &str| f.get(k).and_then(Value::as_str).unwrap_or_default();
            let what = if s("PackageType") == "Image" { "image".to_owned() } else { s("Runtime").to_owned() };
            out.push(json!({ "kind": "Function", "name": s("FunctionName"), "summary": format!("{what} · {} MB", f.get("MemorySize").and_then(Value::as_u64).unwrap_or(0)), "updated": s("LastModified"), "description": s("Description") }));
        }
        marker = page.get("NextMarker").and_then(Value::as_str).filter(|m| !m.is_empty()).map(str::to_owned);
        if marker.is_none() || out.len() >= 1000 {
            break;
        }
    }
    Ok(json!({ "resources": out }))
}

/// A function as it runs now, as the definition that adopts it (§8.2 step 2).
pub async fn adopt(cx: &Cx<'_>, name: &str) -> Result<Value> {
    let Some(answer) = get_function(cx, name).await? else { bail!("no function {name} here") };
    let cfg = answer.get("Configuration").cloned().unwrap_or(Value::Null);
    let mut config = serde_json::Map::new();
    for k in p::CONFIG_FIELDS {
        if let Some(v) = cfg.get(*k).filter(|v| !v.is_null()) {
            config.insert((*k).to_owned(), v.clone());
        }
    }
    if let Some(Value::Array(layers)) = config.get("Layers").cloned() {
        config.insert("Layers".into(), Value::Array(layers.iter().filter_map(|l| l.get("Arn").cloned()).collect()));
    }
    if let Some(vpc) = config.get_mut("VpcConfig").and_then(Value::as_object_mut) {
        vpc.remove("VpcId");
    }
    let image = answer.pointer("/Code/ImageUri").and_then(Value::as_str);
    cx.conn.budget().await;
    let aliases = cx.aws()?.rest("lambda", Method::Get, &format!("{}/aliases", function_path(name)), &[], None).await?;
    let alias = aliases.get("Aliases").and_then(Value::as_array).and_then(|a| a.iter().filter_map(|x| x.get("Name").and_then(Value::as_str)).find(|n| *n == "live").or_else(|| a.first().and_then(|x| x.get("Name").and_then(Value::as_str)))).unwrap_or("live");
    let mut def = json!({ "function": name, "alias": alias, "config": config });
    let note = match (image, def.as_object_mut()) {
        (Some(uri), Some(m)) => {
            m.insert("code".into(), json!(uri));
            None
        }
        (_, Some(m)) => {
            m.insert("code".into(), json!("${{ artifacts.zip }}"));
            Some("Lambda doesn't say where a zip came from: point code at the zip CI builds (an artifact) or an s3:// key")
        }
        _ => None,
    };
    Ok(json!({ "definition": def, "note": note, "live": p::live_function(&answer, None) }))
}

pub async fn apply(cx: &Cx<'_>, desired: &Value) -> Result<Value> {
    match obj::kind(desired) {
        "Function" => apply_function(cx, desired).await,
        "Alias" => apply_alias(cx, desired).await,
        "FunctionUrl" => apply_url(cx, desired).await,
        other => bail!("aws-lambda doesn't apply {other}"),
    }
}

/// Where the function's code is, as Lambda takes it.
fn code_location(desired: &Value) -> Result<Value> {
    let code = desired.pointer("/spec/Code").ok_or_else(|| anyhow::anyhow!("no spec.Code"))?;
    if let Some(a) = code.get("Artifact").and_then(Value::as_str) {
        let source = p::code_source(a).map_err(|e| anyhow::anyhow!("{e}"))?;
        if matches!(source, CodeSource::Artifact(_)) {
            bail!("a zip CI built goes through kiln's bucket in the account, which isn't built yet: give code as s3://bucket/key or an image for now");
        }
    }
    Ok(code.clone())
}

/// Waits until the function settles (it isn't pending, nor updating), failing if Lambda says
/// the change failed.
async fn settle(cx: &Cx<'_>, name: &str) -> Result<Value> {
    until(cx.conn, SETTLE, &format!("function {name} settling"), || async {
        let Some(answer) = get_function(cx, name).await? else { bail!("function {name} is gone") };
        let c = answer.get("Configuration").cloned().unwrap_or(Value::Null);
        let st = |k: &str| c.get(k).and_then(Value::as_str).unwrap_or_default().to_owned();
        match (st("State").as_str(), st("LastUpdateStatus").as_str()) {
            ("Failed", _) => bail!("Lambda failed the function: {}", st("StateReason")),
            (_, "Failed") => bail!("Lambda failed the update: {}", st("LastUpdateStatusReason")),
            ("Pending", _) | (_, "InProgress") => Ok(None),
            _ => Ok(Some(answer)),
        }
    })
    .await
}

async fn apply_function(cx: &Cx<'_>, desired: &Value) -> Result<Value> {
    let name = obj::name(desired);
    let aws = cx.aws()?;
    let code = code_location(desired)?;
    let live = function(cx, name).await?;
    let record = cx.applied(GROUP, "Function", "", name);
    let mut changed = false;
    match &live {
        None => {
            cx.conn.budget().await;
            aws.rest("lambda", Method::Post, FUNCTIONS, &[], Some(&p::create_body(desired, &code))).await?;
            changed = true;
        }
        Some(live) => {
            if !p::code_matches(desired, live) {
                settle(cx, name).await?;
                cx.conn.budget().await;
                aws.rest("lambda", Method::Put, &format!("{}/code", function_path(name)), &[], Some(&p::code_body(desired, &code))).await?;
                changed = true;
            }
            if !p::configuration_matches(desired, live) {
                settle(cx, name).await?;
                cx.conn.budget().await;
                aws.rest("lambda", Method::Put, &format!("{}/configuration", function_path(name)), &[], Some(&p::configuration_body(desired))).await?;
                changed = true;
            }
        }
    }
    let settled = settle(cx, name).await?;
    let arn = settled.pointer("/Configuration/FunctionArn").and_then(Value::as_str).unwrap_or_default().to_owned();
    // Tags: kiln's tracking, and the definition's own.
    let want = p::tags_of(desired);
    let have = settled.get("Tags").and_then(Value::as_object).cloned().unwrap_or_default();
    if !arn.is_empty() && want.iter().any(|(k, v)| have.get(k) != Some(v)) {
        cx.conn.budget().await;
        aws.rest("lambda", Method::Post, &format!("/2017-03-31/tags/{}", encode(&arn, false)), &[], Some(&json!({ "Tags": want }))).await?;
    }
    // A version for each change, and one for a function kiln takes over.
    let sha = settled.pointer("/Configuration/CodeSha256").and_then(Value::as_str).unwrap_or_default().to_owned();
    let mut version = record.as_ref().and_then(|r| r.made.get("Version").and_then(Value::as_str).map(str::to_owned));
    if changed || version.is_none() {
        cx.conn.budget().await;
        let v = aws.rest("lambda", Method::Post, &format!("{}/versions", function_path(name)), &[], Some(&json!({ "CodeSha256": sha }))).await?;
        version = v.get("Version").and_then(Value::as_str).map(str::to_owned);
    }
    cx.record(GROUP, "Function", "", name, Some(applied(desired, json!({ "CodeSha256": sha, "Version": version, "FunctionArn": arn })))).await?;
    function(cx, name).await?.ok_or_else(|| anyhow::anyhow!("function {name} is gone"))
}

async fn apply_alias(cx: &Cx<'_>, desired: &Value) -> Result<Value> {
    let (function, name) = split(obj::name(desired))?;
    let want = desired.pointer("/spec/FunctionVersion").and_then(Value::as_str).unwrap_or(PUBLISHED);
    let version = if want == PUBLISHED {
        published(cx, function).await?.ok_or_else(|| anyhow::anyhow!("function {function} has no published version yet: apply the function first"))?
    } else {
        want.to_owned()
    };
    let mut body = json!({ "FunctionVersion": version });
    if let (Some(d), Value::Object(m)) = (desired.pointer("/spec/Description"), &mut body) {
        m.insert("Description".into(), d.clone());
    }
    if let Value::Object(m) = &mut body {
        // Weights only while a rollout sets them (§5); otherwise none.
        m.insert("RoutingConfig".into(), desired.pointer("/spec/RoutingConfig").cloned().unwrap_or(json!({ "AdditionalVersionWeights": {} })));
    }
    let aws = cx.aws()?;
    let path = format!("{}/aliases", function_path(function));
    cx.conn.budget().await;
    let answer = match aws.rest("lambda", Method::Get, &format!("{path}/{}", encode(name, false)), &[], None).await {
        Ok(_) => {
            cx.conn.budget().await;
            aws.rest("lambda", Method::Put, &format!("{path}/{}", encode(name, false)), &[], Some(&body)).await?
        }
        Err(e) if AwsError::is_not_found(&e) => {
            if let Value::Object(m) = &mut body {
                m.insert("Name".into(), Value::String(name.to_owned()));
            }
            cx.conn.budget().await;
            aws.rest("lambda", Method::Post, &path, &[], Some(&body)).await?
        }
        Err(e) => return Err(e),
    };
    cx.record(GROUP, "Alias", "", obj::name(desired), Some(applied(desired, json!({ "AliasArn": answer.get("AliasArn"), "FunctionVersion": version })))).await?;
    alias(cx, function, name).await?.ok_or_else(|| anyhow::anyhow!("alias {name} is gone"))
}

async fn apply_url(cx: &Cx<'_>, desired: &Value) -> Result<Value> {
    let (function, qualifier) = split(obj::name(desired))?;
    let spec = desired.get("spec").cloned().unwrap_or(Value::Null);
    let body: serde_json::Map<String, Value> = ["AuthType", "Cors", "InvokeMode"].iter().filter_map(|k| spec.get(*k).map(|v| ((*k).to_owned(), v.clone()))).collect();
    let aws = cx.aws()?;
    let path = format!("{URLS}/{}/url", encode(function, false));
    let q = [("Qualifier".to_owned(), qualifier.to_owned())];
    let exists = url(cx, function, qualifier).await?.is_some();
    cx.conn.budget().await;
    let method = if exists { Method::Put } else { Method::Post };
    aws.rest("lambda", method, &path, &q, Some(&Value::Object(body.clone()))).await?;
    // A public URL needs the function's policy to let anyone invoke it through the URL.
    if body.get("AuthType").and_then(Value::as_str) == Some("NONE") {
        let statement = json!({ "StatementId": "kiln-function-url", "Action": "lambda:InvokeFunctionUrl", "Principal": "*", "FunctionUrlAuthType": "NONE" });
        cx.conn.budget().await;
        match aws.rest("lambda", Method::Post, &format!("{}/policy", function_path(function)), &q, Some(&statement)).await {
            Ok(_) => {}
            Err(e) if e.downcast_ref::<AwsError>().is_some_and(|a| a.code == "ResourceConflictException") => {}
            Err(e) => return Err(e),
        }
    }
    cx.record(GROUP, "FunctionUrl", "", obj::name(desired), Some(applied(desired, Value::Null))).await?;
    url(cx, function, qualifier).await?.ok_or_else(|| anyhow::anyhow!("the URL of {function}:{qualifier} is gone"))
}

pub async fn delete(cx: &Cx<'_>, key: &ResKey) -> Result<()> {
    let aws = cx.aws()?;
    let name: &str = &key.name;
    let (path, q) = match &*key.kind {
        "Function" => (function_path(name), Vec::new()),
        "Alias" => {
            let (f, a) = split(name)?;
            (format!("{}/aliases/{}", function_path(f), encode(a, false)), Vec::new())
        }
        "FunctionUrl" => {
            let (f, q) = split(name)?;
            (format!("{URLS}/{}/url", encode(f, false)), vec![("Qualifier".to_owned(), q.to_owned())])
        }
        other => bail!("aws-lambda doesn't delete {other}"),
    };
    cx.conn.budget().await;
    match aws.rest("lambda", Method::Delete, &path, &q, None).await {
        Ok(_) => Ok(()),
        Err(e) if AwsError::is_not_found(&e) => Ok(()),
        Err(e) => Err(e),
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use kiln_cd::providers::{RenderInput, Target};
    use serde_json::Map;

    use super::*;
    use crate::cd::targets::fake::{self, Fake, MemRecords};
    use crate::cd::targets::{Conn, Creds, Cx, key_of};
    use crate::ci::integrations::aws::{self, Aws, Credentials};

    fn desired(code: &str, memory: u32) -> anyhow::Result<Vec<Value>> {
        let spec = json!({ "function": "thumbs", "code": code, "set": { "Runtime": "python3.13", "Handler": "app.handler", "Role": "arn:aws:iam::1:role/thumbs", "MemorySize": memory }, "url": { "AuthType": "NONE" } });
        let none = Map::new();
        let objs = p::render(&RenderInput { spec: &spec, target: &none, app: "thumbs", namespace: "", revision: "abc", read: &|_| None }).map_err(|e| anyhow::anyhow!("{e}"))?;
        Ok(objs.into_iter().map(|o| fake::tracked(o, "default/thumbs-production")).collect())
    }

    /// A function's life on a faked Lambda: created (waiting while it's pending) and published,
    /// its alias on the version kiln published; changed (code, then configuration, each waiting
    /// for the one before) and published again, the alias moving; drift read as drift; a public
    /// URL with its permission; then deleted.
    #[test]
    fn functions_publish_and_aliases_follow() -> anyhow::Result<()> {
        crate::rt::block_on(async {
            let fake = Arc::new(Fake::default());
            let base = fake::start(fake.clone()).await?;
            aws::override_endpoint("lambda", &base);
            let target = Target::parse("aws-lambda/aws-test/us-east-1").ok_or_else(|| anyhow::anyhow!("target"))?;
            let def = target.def().ok_or_else(|| anyhow::anyhow!("provider"))?;
            let conn = Conn { settle_every: std::time::Duration::from_millis(5), gap: std::time::Duration::ZERO, ..Conn::new(target, def) };
            let records = MemRecords::default();
            let creds = Credentials { access_key_id: "AKIDFAKE".into(), secret_access_key: "s".into(), session_token: Some("t".into()), expires_at: None };
            let cx = Cx { conn: &conn, creds: Creds::Aws(Aws { creds, region: "us-east-1".into() }), records: &records };

            let objs = desired("s3://bucket/thumbs/v1.zip", 512)?;
            let (f, a, u) = (objs.first().cloned().unwrap_or_default(), objs.get(1).cloned().unwrap_or_default(), objs.get(2).cloned().unwrap_or_default());
            let live = cx.with_applied(apply(&cx, &f).await?);
            assert_eq!(live.pointer("/spec/Code"), f.pointer("/spec/Code"), "code reads as kiln applied it");
            assert_eq!(obj::label(&live, kiln_cd::engine::APP_LABEL), obj::label(&f, kiln_cd::engine::APP_LABEL));
            assert!(obj::annotation(&live, kiln_cd::diff::LAST_APPLIED).is_some());
            let la = apply(&cx, &a).await?;
            assert_eq!(la.pointer("/spec/FunctionVersion"), Some(&json!(PUBLISHED)));
            let lu = apply(&cx, &u).await?;
            assert!(lu.pointer("/status/FunctionUrl").and_then(Value::as_str).is_some_and(|s| s.starts_with("https://thumbs.")));
            // Applying again changes nothing: no update, no new version, and the permission
            // already there is fine.
            apply(&cx, &f).await?;
            apply(&cx, &u).await?;
            assert_eq!(fake.state.lock().calls, ["CreateFunction thumbs", "PublishVersion thumbs 1", "CreateAlias thumbs:live"]);

            // New code and more memory: two updates, each after the other settled; version 2.
            let objs = desired("s3://bucket/thumbs/v2.zip", 1024)?;
            let (f2, a2) = (objs.first().cloned().unwrap_or_default(), objs.get(1).cloned().unwrap_or_default());
            apply(&cx, &f2).await?;
            let names = [(String::new(), "thumbs:live".to_owned())];
            let before = list(&cx, "Alias", &names).await?;
            assert_eq!(before.first().and_then(|o| o.pointer("/spec/FunctionVersion")), Some(&json!("1")), "the alias is behind until it moves");
            apply(&cx, &a2).await?;
            let after = list(&cx, "Alias", &names).await?;
            assert_eq!(after.first().and_then(|o| o.pointer("/spec/FunctionVersion")), Some(&json!(PUBLISHED)));
            let calls = fake.state.lock().calls.clone();
            assert_eq!(calls.get(3..), Some(&["UpdateFunctionCode thumbs".to_owned(), "UpdateFunctionConfiguration thumbs".to_owned(), "PublishVersion thumbs 2".to_owned(), "UpdateAlias thumbs:live 2".to_owned()][..]));

            // Someone changes the code by hand: it reads as another hash, which kiln's doesn't cover.
            if let Some(func) = fake.state.lock().functions.get_mut("thumbs") {
                func.sha = "handmade=".into();
            }
            let drifted = list(&cx, "Function", &[(String::new(), "thumbs".to_owned())]).await?;
            let d = drifted.first().cloned().unwrap_or_default();
            assert_eq!(d.pointer("/spec/Code"), Some(&json!({ "CodeSha256": "handmade=" })));
            assert!(!p::code_matches(&f2, &d));

            // What the account holds, and the function adopted as it runs.
            let found = discover(&cx).await?;
            assert_eq!(found.pointer("/resources/0/name"), Some(&json!("thumbs")));
            let adopted = adopt(&cx, "thumbs").await?;
            assert_eq!((adopted.pointer("/definition/alias"), adopted.pointer("/definition/config/MemorySize")), (Some(&json!("live")), Some(&json!(1024))));
            assert!(adopted.get("note").is_some_and(|n| n.as_str().is_some_and(|n| n.contains("zip"))));

            // Gone: the alias, then the function; their records with them.
            delete(&cx, &key_of(&a2)).await?;
            delete(&cx, &key_of(&f2)).await?;
            assert!(list(&cx, "Function", &[(String::new(), "thumbs".to_owned())]).await?.is_empty());
            Ok(())
        })
    }
}
