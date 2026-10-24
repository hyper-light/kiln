//! kiln's bucket in an AWS account (docs/cd-targets.md §7): where Lambda zips and Beanstalk
//! bundles CI built go, under content-addressed keys, so a revision uploads only what's new and
//! a rollback uploads nothing. One bucket per account and region, `kiln-<account>-<region>`,
//! private (S3 blocks public access on new buckets).

use anyhow::{Result, bail};
use serde_json::Value;

use super::Cx;
use crate::cd::artifacts::Files;
use crate::ci::integrations::aws::{Body, Call, endpoint, encode};
use crate::http::client::Method;

/// kiln's bucket for an account and region.
pub fn bucket_name(account: &str, region: &str) -> String {
    format!("kiln-{account}-{region}")
}

/// The account the target's credentials act in, asked once.
async fn account(cx: &Cx<'_>) -> Result<String> {
    if let Some(a) = cx.conn.memo.lock().get("aws:account").cloned() {
        return Ok(a);
    }
    cx.conn.budget().await;
    let a = cx.aws()?.account().await?;
    cx.conn.memo.lock().insert("aws:account".into(), a.clone());
    Ok(a)
}

async fn status(cx: &Cx<'_>, method: Method, path: &str, body: Body) -> Result<(u16, String)> {
    let aws = cx.aws()?;
    let base = endpoint("s3", &aws.region);
    cx.conn.budget().await;
    let resp = aws.send(Call { service: "s3", base: &base, method, path, query: &[], headers: Vec::new(), body }).await?;
    let code = resp.status();
    let text = if method == Method::Head { String::new() } else { String::from_utf8_lossy(&resp.bytes(1 << 20).await?).into_owned() };
    Ok((code, text))
}

/// kiln's bucket in the target's account and region, made if it isn't there yet.
pub async fn bucket(cx: &Cx<'_>) -> Result<String> {
    let region = cx.conn.region()?.to_owned();
    let name = bucket_name(&account(cx).await?, &region);
    let memo = format!("s3:bucket:{name}");
    if cx.conn.memo.lock().contains_key(&memo) {
        return Ok(name);
    }
    let path = format!("/{name}");
    let (code, _) = status(cx, Method::Head, &path, Vec::new().into()).await?;
    if code == 404 {
        // us-east-1 takes no location; every other region names itself.
        let body = if region == "us-east-1" { Vec::new() } else { format!("<CreateBucketConfiguration xmlns=\"http://s3.amazonaws.com/doc/2006-03-01/\"><LocationConstraint>{region}</LocationConstraint></CreateBucketConfiguration>").into_bytes() };
        let (code, text) = status(cx, Method::Put, &path, body.into()).await?;
        if !(200..300).contains(&code) && !text.contains("BucketAlreadyOwnedByYou") {
            bail!("making kiln's bucket {name}: HTTP {code} {}", crate::ci::integrations::aws::xml_text(&text, "Message").unwrap_or(text));
        }
    } else if !(200..300).contains(&code) {
        bail!("kiln's bucket {name} answers HTTP {code}: the role needs s3:CreateBucket, s3:ListBucket, s3:GetObject and s3:PutObject on it");
    }
    cx.conn.memo.lock().insert(memo, "ok".into());
    Ok(name)
}

/// An artifact in kiln's bucket, uploaded unless it already is: its bucket and key.
pub async fn artifact(cx: &Cx<'_>, files: &dyn Files, prefix: &str, hex: &str, extension: &str) -> Result<(String, String)> {
    let bucket = bucket(cx).await?;
    let key = format!("kiln/{prefix}/{hex}{extension}");
    let path = format!("/{bucket}/{}", encode(&key, true));
    let (code, _) = status(cx, Method::Head, &path, Vec::new().into()).await?;
    if code != 200 {
        let (rx, len) = files.open(hex).await?;
        let (code, text) = status(cx, Method::Put, &path, Body::Stream { rx, len, sha256: hex.to_owned() }).await?;
        if !(200..300).contains(&code) {
            bail!("uploading {key} to {bucket}: HTTP {code} {}", crate::ci::integrations::aws::xml_text(&text, "Message").unwrap_or(text));
        }
    }
    Ok((bucket, key))
}

/// `bytes` in kiln's bucket under `kiln/<prefix>/<sha256><extension>`, uploaded unless they
/// already are: its bucket and key (a CloudFormation template too long to send inline).
pub async fn put(cx: &Cx<'_>, prefix: &str, bytes: &[u8], extension: &str) -> Result<(String, String)> {
    let bucket = bucket(cx).await?;
    let hex = crate::util::sha256_hex(bytes);
    let key = format!("kiln/{prefix}/{hex}{extension}");
    let path = format!("/{bucket}/{}", encode(&key, true));
    let (code, _) = status(cx, Method::Head, &path, Vec::new().into()).await?;
    if code != 200 {
        let (code, text) = status(cx, Method::Put, &path, bytes.to_vec().into()).await?;
        if !(200..300).contains(&code) {
            bail!("uploading {key} to {bucket}: HTTP {code} {}", crate::ci::integrations::aws::xml_text(&text, "Message").unwrap_or(text));
        }
    }
    Ok((bucket, key))
}

/// The HTTPS address of an object in kiln's bucket, as CloudFormation takes a template's.
pub fn url(cx: &Cx<'_>, bucket: &str, key: &str) -> Result<String> {
    let region = cx.conn.region()?;
    Ok(format!("https://{bucket}.s3.{region}.amazonaws.com/{}", encode(key, true)))
}

/// Where code an artifact holds is, as Lambda (and Beanstalk) take it.
pub async fn code(cx: &Cx<'_>, files: &dyn Files, hex: &str) -> Result<Value> {
    let (bucket, key) = artifact(cx, files, "lambda", hex, ".zip").await?;
    Ok(serde_json::json!({ "S3Bucket": bucket, "S3Key": key }))
}
