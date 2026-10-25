//! What the `azure-*` providers share (docs/cd-targets.md §3): Resource Manager's paths, waiting
//! for provisioning and long-running operations, and kiln's storage account, where what Azure
//! fetches by URL (a site's zip) waits.

use std::time::Duration;

use anyhow::{Result, bail};
use serde_json::{Value, json};

use super::{Cx, until};
use crate::ci::integrations::azure::{self, Answer, AzureError, Body};
use crate::http::client::Method;

/// The longest a change takes to settle on Azure: a revision starting, a rolling upgrade.
pub const SETTLE: Duration = Duration::from_secs(3600);

const GROUPS_API: &str = "2021-04-01";
const STORAGE_API: &str = "2023-05-01";

/// The target's resource group: `/subscriptions/<id>/resourceGroups/<name>`.
pub fn group_path(cx: &Cx<'_>) -> Result<String> {
    Ok(format!("/subscriptions/{}/resourceGroups/{}", cx.place("subscription")?, cx.place("resource-group")?))
}

/// `Ok(None)` for Azure saying it isn't there.
pub fn missing<T>(r: Result<T>) -> Result<Option<T>> {
    match r {
        Ok(v) => Ok(Some(v)),
        Err(e) if AzureError::is_not_found(&e) => Ok(None),
        Err(e) => Err(e),
    }
}

/// A resource as it is, or none.
pub async fn get(cx: &Cx<'_>, path: &str, api: &str) -> Result<Option<Value>> {
    cx.conn.budget().await;
    missing(cx.arm()?.call(Method::Get, path, api, None).await)
}

/// A call, spaced by the target's budget: the whole answer.
pub async fn call(cx: &Cx<'_>, method: Method, path: &str, api: &str, body: Option<&Value>) -> Result<Answer> {
    cx.conn.budget().await;
    cx.arm()?.answer(method, path, api, body).await
}

/// Waits for a resource's provisioning to end: the resource as it ended; why, if it failed.
pub async fn provisioned(cx: &Cx<'_>, path: &str, api: &str, what: &str) -> Result<Value> {
    let r = until(cx.conn, SETTLE, what, || async {
        let r = cx.arm()?.call(Method::Get, path, api, None).await?;
        let state = r.pointer("/properties/provisioningState").and_then(Value::as_str).unwrap_or("Succeeded");
        Ok(matches!(state, "Succeeded" | "Failed" | "Canceled").then_some(r))
    })
    .await?;
    match r.pointer("/properties/provisioningState").and_then(Value::as_str) {
        Some(s @ ("Failed" | "Canceled")) => bail!("{what}: Azure says {s}"),
        _ => Ok(r),
    }
}

/// Waits for the long-running operation an answer points at (`Azure-AsyncOperation`, or
/// `Location`); why, if it failed.
pub async fn operation(cx: &Cx<'_>, a: &Answer, what: &str) -> Result<()> {
    if let Some(url) = a.header("Azure-AsyncOperation").map(str::to_owned) {
        let done = until(cx.conn, SETTLE, what, || async {
            let s = cx.arm()?.send(Method::Get, &url, Body::None, &[], true).await?.body;
            Ok(matches!(s.get("status").and_then(Value::as_str), Some("Succeeded" | "Failed" | "Canceled")).then_some(s))
        })
        .await?;
        if done.get("status").and_then(Value::as_str) != Some("Succeeded") {
            bail!("{what}: {}", done.pointer("/error/message").and_then(Value::as_str).unwrap_or("it failed"));
        }
    } else if let (202, Some(url)) = (a.status, a.header("Location").map(str::to_owned)) {
        until(cx.conn, SETTLE, what, || async { Ok((cx.arm()?.send(Method::Get, &url, Body::None, &[], true).await?.status != 202).then_some(())) }).await?;
    }
    Ok(())
}

/// The resource group's location, where what kiln makes in it goes.
pub async fn location(cx: &Cx<'_>) -> Result<String> {
    let g = get(cx, &group_path(cx)?, GROUPS_API).await?.ok_or_else(|| anyhow::anyhow!("no resource group {}", cx.place("resource-group").unwrap_or_default()))?;
    g.get("location").and_then(Value::as_str).map(str::to_owned).ok_or_else(|| anyhow::anyhow!("the resource group has no location"))
}

/// kiln's tags on what it applies: Azure's tag names hold no `/`, so `kiln-app` and
/// `kiln-tracking`.
pub fn tags(desired: &Value) -> serde_json::Map<String, Value> {
    let mut t = serde_json::Map::new();
    if let Some(v) = kiln_cd::obj::label(desired, kiln_cd::engine::APP_LABEL) {
        t.insert("kiln-app".into(), json!(v));
    }
    if let Some(v) = kiln_cd::obj::annotation(desired, kiln_cd::engine::TRACKING) {
        t.insert("kiln-tracking".into(), json!(v.chars().take(256).collect::<String>()));
    }
    t
}

// ------------------------------------------------------------------ kiln's storage account

/// The storage account's name kiln keeps in a resource group: `kiln<hash>`.
pub fn storage_name(group: &str) -> String {
    format!("kiln{}", &format!("{:016x}", kiln_cd::obj::hash_bytes(group.as_bytes()))[..16])
}

/// A storage account (`account`, or kiln's own, made if it isn't there) with its `kiln`
/// container: the account's name.
pub async fn storage(cx: &Cx<'_>, account: Option<&str>) -> Result<String> {
    let group = group_path(cx)?;
    let name = account.map_or_else(|| storage_name(&group), str::to_owned);
    let path = format!("{group}/providers/Microsoft.Storage/storageAccounts/{name}");
    if get(cx, &path, STORAGE_API).await?.is_none() {
        if account.is_some() {
            bail!("no storage account {name} in the resource group");
        }
        let body = json!({ "location": location(cx).await?, "kind": "StorageV2", "sku": { "name": "Standard_LRS" }, "tags": { "kiln": "artifacts" },
            "properties": { "allowBlobPublicAccess": false, "minimumTlsVersion": "TLS1_2", "supportsHttpsTrafficOnly": true } });
        let a = call(cx, Method::Put, &path, STORAGE_API, Some(&body)).await?;
        operation(cx, &a, &format!("making storage account {name}")).await?;
        provisioned(cx, &path, STORAGE_API, &format!("making storage account {name}")).await?;
    }
    let container = format!("{path}/blobServices/default/containers/kiln");
    if get(cx, &container, STORAGE_API).await?.is_none() {
        call(cx, Method::Put, &container, STORAGE_API, Some(&json!({ "properties": { "publicAccess": "None" } }))).await?;
    }
    Ok(name)
}

/// When a SAS made now for `secs` ends, as Azure writes it.
fn expiry(secs: i64) -> String {
    let t = time::OffsetDateTime::from_unix_timestamp(crate::util::now_unix() + secs).unwrap_or(time::OffsetDateTime::UNIX_EPOCH);
    t.replace_nanosecond(0).unwrap_or(t).format(&time::format_description::well_known::Rfc3339).unwrap_or_default()
}

/// Puts an artifact (by its SHA-256, in hex) in the account's `kiln` container as
/// `<prefix>/<hex><extension>`, unless it's there: its blob's name. Signed with an account SAS
/// Resource Manager makes for the hour, so no key is held.
pub async fn put_blob(cx: &Cx<'_>, account: &str, prefix: &str, hex: &str, extension: &str) -> Result<String> {
    let blob = format!("{prefix}/{hex}{extension}");
    let path = format!("{}/providers/Microsoft.Storage/storageAccounts/{account}/ListAccountSas", group_path(cx)?);
    let sas = call(cx, Method::Post, &path, STORAGE_API, Some(&json!({ "signedServices": "b", "signedResourceTypes": "o", "signedPermission": "rcw", "signedProtocol": "https", "signedExpiry": expiry(3600) }))).await?;
    let token = sas.body.get("accountSasToken").and_then(Value::as_str).ok_or_else(|| anyhow::anyhow!("Azure made no SAS for {account}"))?;
    let url = format!("{}/kiln/{blob}?{token}", azure::host(&format!("{account}.blob.core.windows.net")));
    cx.conn.budget().await;
    let there = missing(cx.arm()?.send(Method::Head, &url, Body::None, &[("x-ms-version", "2023-11-03")], false).await)?.is_some();
    if !there {
        let (rx, len) = cx.files.open(hex).await?;
        cx.conn.budget().await;
        cx.arm()?.send(Method::Put, &url, Body::Stream { rx, len, content_type: "application/zip".into() }, &[("x-ms-blob-type", "BlockBlob"), ("x-ms-version", "2023-11-03")], false).await?;
    }
    Ok(blob)
}

/// A URL Azure reads a blob at, for `secs`: a service SAS Resource Manager makes for that blob
/// alone, to read.
pub async fn blob_url(cx: &Cx<'_>, account: &str, blob: &str, secs: i64) -> Result<String> {
    let path = format!("{}/providers/Microsoft.Storage/storageAccounts/{account}/ListServiceSas", group_path(cx)?);
    let body = json!({ "canonicalizedResource": format!("/blob/{account}/kiln/{blob}"), "signedResource": "b", "signedPermission": "r", "signedProtocol": "https", "signedExpiry": expiry(secs) });
    let sas = call(cx, Method::Post, &path, STORAGE_API, Some(&body)).await?;
    let token = sas.body.get("serviceSasToken").and_then(Value::as_str).ok_or_else(|| anyhow::anyhow!("Azure made no SAS for {blob}"))?;
    Ok(format!("{}/kiln/{blob}?{token}", azure::host(&format!("{account}.blob.core.windows.net"))))
}
