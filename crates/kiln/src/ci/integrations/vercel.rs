//! Vercel (docs/ci.md W11; docs/cd-targets.md §6): its REST API with an access token, which
//! kiln keeps in its secret store; a team's calls carry its `teamId`. Errors keep Vercel's code
//! and message. Files upload by their SHA-1, as Vercel addresses them.

use std::sync::OnceLock;

use anyhow::{Context as _, Result};
use serde_json::Value;

use crate::http::client::{Client, Method, Roots};

/// An error Vercel answered.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct VercelError {
    pub status: u16,
    pub code: String,
    pub message: String,
    /// The whole error: `missing_files` lists the files Vercel lacks in `missing`.
    pub detail: Value,
}

impl std::fmt::Display for VercelError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Vercel {}: {} (HTTP {})", self.code, self.message, self.status)
    }
}

impl std::error::Error for VercelError {}

impl VercelError {
    /// Whether `e` is Vercel saying something doesn't exist.
    pub fn is_not_found(e: &anyhow::Error) -> bool {
        e.downcast_ref::<VercelError>().is_some_and(|v| v.status == 404 || v.code == "not_found")
    }
}

fn client() -> Result<&'static Client> {
    static C: OnceLock<Client> = OnceLock::new();
    if let Some(c) = C.get() {
        return Ok(c);
    }
    let c = Client::new(Roots::WebPki, None)?;
    Ok(C.get_or_init(|| c))
}

/// The API's base (`https://api.vercel.com`), or a test's fake.
pub fn base() -> String {
    super::endpoint("vercel", || "https://api.vercel.com".into())
}

/// A file's SHA-1, in hex: how Vercel names uploads.
pub fn sha1_hex(data: &[u8]) -> String {
    hex::encode(aws_lc_rs::digest::digest(&aws_lc_rs::digest::SHA1_FOR_LEGACY_USE_ONLY, data).as_ref())
}

/// Vercel's API, with a token, for a team or a personal account.
pub struct Vc<'a> {
    pub token: &'a str,
    pub team: Option<&'a str>,
}

impl Vc<'_> {
    fn url(&self, path: &str) -> String {
        match self.team.filter(|t| !t.is_empty()) {
            Some(t) => format!("{}{path}{}teamId={}", base(), if path.contains('?') { "&" } else { "?" }, super::aws::encode(t, false)),
            None => format!("{}{path}", base()),
        }
    }

    /// A call's answer, or the error Vercel gave.
    pub async fn call(&self, method: Method, path: &str, body: Option<&Value>) -> Result<Value> {
        let mut req = client()?.request(method, &self.url(path)).timeout(std::time::Duration::from_secs(60)).bearer(self.token);
        if let Some(b) = body {
            req = req.json(b);
        }
        let resp = req.send().await.with_context(|| format!("Vercel {path}"))?;
        let status = resp.status();
        let bytes = resp.bytes(32 << 20).await?;
        let v: Value = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
        if (200..300).contains(&status) {
            return Ok(v);
        }
        let e = v.get("error").cloned().unwrap_or(Value::Null);
        let message = e.get("message").and_then(Value::as_str).map(str::to_owned).unwrap_or_else(|| String::from_utf8_lossy(&bytes).chars().take(300).collect());
        Err(VercelError { status, code: e.get("code").and_then(Value::as_str).unwrap_or("error").to_owned(), message, detail: e }.into())
    }

    /// Uploads one file by its SHA-1 (`POST /v2/files`).
    pub async fn upload(&self, sha1: &str, data: Vec<u8>) -> Result<()> {
        let len = data.len().to_string();
        let resp = client()?
            .request(Method::Post, &self.url("/v2/files"))
            .timeout(std::time::Duration::from_secs(300))
            .bearer(self.token)
            .header("x-vercel-digest", sha1)
            .header("Content-Type", "application/octet-stream")
            .header("Content-Length", &len)
            .body(data)
            .send()
            .await
            .context("Vercel /v2/files")?;
        let status = resp.status();
        if (200..300).contains(&status) {
            return Ok(());
        }
        let v: Value = serde_json::from_slice(&resp.bytes(1 << 20).await?).unwrap_or(Value::Null);
        let e = v.get("error").cloned().unwrap_or(Value::Null);
        Err(VercelError { status, code: e.get("code").and_then(Value::as_str).unwrap_or("error").to_owned(), message: e.get("message").and_then(Value::as_str).unwrap_or("upload failed").to_owned(), detail: e }.into())
    }
}

/// Whether a token works, for its team when it names one.
pub async fn verify(token: &str, team: Option<&str>) -> Result<String> {
    let vc = Vc { token, team: None };
    let user = vc.call(Method::Get, "/v2/user", None).await?;
    let who = user.pointer("/user/username").and_then(Value::as_str).unwrap_or("someone").to_owned();
    match team.filter(|t| !t.is_empty()) {
        Some(t) => {
            let team = vc.call(Method::Get, &format!("/v2/teams/{}", super::aws::encode(t, false)), None).await?;
            Ok(format!("signed in as {who}, in team {}", team.get("name").and_then(Value::as_str).unwrap_or(t)))
        }
        None => Ok(format!("signed in as {who}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn files_are_named_by_sha1() {
        assert_eq!(sha1_hex(b"abc"), "a9993e364706816aba3e25717850c26c9cd0d89d");
    }
}
