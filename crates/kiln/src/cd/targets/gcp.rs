//! What the `gcp-*` providers share (docs/cd-targets.md §3): Google's long-running operations,
//! what isn't there, and Cloud Storage, where App Engine reads what it deploys.

use std::time::Duration;

use anyhow::{Result, bail};
use serde_json::Value;

use super::{Cx, until};
use crate::ci::integrations::gcp::{self, GcpError};
use crate::http::client::Method;

/// The longest a change takes to settle on Google Cloud: a build, a revision starting.
pub const SETTLE: Duration = Duration::from_secs(1800);

/// `Ok(None)` for Google saying it isn't there.
pub fn missing<T>(r: Result<T>) -> Result<Option<T>> {
    match r {
        Ok(v) => Ok(Some(v)),
        Err(e) if GcpError::is_not_found(&e) => Ok(None),
        Err(e) => Err(e),
    }
}

/// `path` of `api` (`/v2/…`) as it is, or none.
pub async fn get(cx: &Cx<'_>, api: &str, path: &str) -> Result<Option<Value>> {
    cx.conn.budget().await;
    missing(cx.gcp()?.call(api, Method::Get, path, &[], None).await)
}

/// A call that changes something, spaced by the target's budget.
pub async fn call(cx: &Cx<'_>, api: &str, method: Method, path: &str, query: &[(&str, &str)], body: Option<&Value>) -> Result<Value> {
    cx.conn.budget().await;
    cx.gcp()?.call(api, method, path, query, body).await
}

/// Waits for one of Google's long-running operations (`google.longrunning.Operation`, at
/// `/<version>/<its name>`): its response, or why it failed.
pub async fn wait(cx: &Cx<'_>, api: &str, version: &str, op: Value, what: &str) -> Result<Value> {
    let done = |o: &Value| o.get("done").and_then(Value::as_bool) == Some(true);
    let op = if done(&op) {
        op
    } else {
        let name = op.get("name").and_then(Value::as_str).ok_or_else(|| anyhow::anyhow!("{what}: Google answered no operation"))?.to_owned();
        let path = format!("/{version}/{name}");
        until(cx.conn, SETTLE, what, || async { Ok(cx.gcp()?.call(api, Method::Get, &path, &[], None).await.map(|o| done(&o).then_some(o))?) }).await?
    };
    if let Some(e) = op.get("error") {
        bail!("{what}: {}", e.get("message").and_then(Value::as_str).unwrap_or("it failed"));
    }
    Ok(op.get("response").cloned().unwrap_or(Value::Null))
}

/// Puts an artifact (by its SHA-256, in hex) in a Cloud Storage bucket as `object`, unless it's
/// there: its `gs://` place.
pub async fn put_object(cx: &Cx<'_>, bucket: &str, object: &str, hex: &str, content_type: &str) -> Result<String> {
    let enc = |s: &str| crate::ci::integrations::aws::encode(s, false);
    if get(cx, "storage", &format!("/storage/v1/b/{}/o/{}", enc(bucket), enc(object))).await?.is_none() {
        let (rx, len) = cx.files.open(hex).await?;
        let url = format!("{}/upload/storage/v1/b/{}/o?uploadType=media&name={}", gcp::base("storage"), enc(bucket), enc(object));
        cx.conn.budget().await;
        cx.gcp()?.upload(Method::Post, &url, &[("Content-Type", content_type)], rx, len, true).await?;
    }
    Ok(format!("gs://{bucket}/{object}"))
}
