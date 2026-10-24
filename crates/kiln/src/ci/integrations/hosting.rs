//! The hosting platforms kiln deploys to with a stored token (docs/ci.md W11; docs/cd-targets.md
//! §3): Render, Netlify, Heroku, DigitalOcean and Fly.io. Their REST APIs take the token as a
//! bearer; errors keep the platform's code and message. The token never leaves kiln's secret
//! store but for these calls, and is never printed.

use std::sync::OnceLock;

use anyhow::{Context as _, Result};
use serde_json::Value;

use crate::http::client::{Client, Method, Roots};

/// An error a platform answered.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HostingError {
    pub status: u16,
    pub code: String,
    pub message: String,
}

impl std::fmt::Display for HostingError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}: {} (HTTP {})", self.code, self.message, self.status)
    }
}

impl std::error::Error for HostingError {}

impl HostingError {
    /// Whether `e` is a platform saying something doesn't exist.
    pub fn is_not_found(e: &anyhow::Error) -> bool {
        e.downcast_ref::<HostingError>().is_some_and(|h| h.status == 404 || h.code == "not_found")
    }
}

/// The platforms' error shapes: `{id, message}` (Heroku, DigitalOcean, Render), `{code,
/// message}` (Netlify), `{error}` (Fly.io).
pub fn error_of(status: u16, body: &[u8]) -> HostingError {
    let v: Value = serde_json::from_slice(body).unwrap_or(Value::Null);
    let text = |k: &str| v.get(k).and_then(|x| x.as_str().map(str::to_owned).or_else(|| x.as_i64().map(|n| n.to_string())));
    let message = text("message").or_else(|| text("error")).unwrap_or_else(|| String::from_utf8_lossy(body).chars().take(300).collect());
    HostingError { status, code: text("id").or_else(|| text("code")).unwrap_or_else(|| "error".into()), message }
}

fn client() -> Result<&'static Client> {
    static C: OnceLock<Client> = OnceLock::new();
    if let Some(c) = C.get() {
        return Ok(c);
    }
    let c = Client::new(Roots::WebPki, None)?;
    Ok(C.get_or_init(|| c))
}

/// A platform's API base, or a test's fake.
pub fn base(kind: &str) -> String {
    super::endpoint(kind, || {
        match kind {
            "render" => "https://api.render.com",
            "netlify" => "https://api.netlify.com",
            "heroku" => "https://api.heroku.com",
            "digitalocean" => "https://api.digitalocean.com",
            "fly" => "https://api.machines.dev",
            _ => "https://invalid.invalid",
        }
        .into()
    })
}

/// A platform's API, with its token.
pub struct Api<'a> {
    pub kind: &'a str,
    pub token: &'a str,
}

impl Api<'_> {
    /// `method` on `path` (under the API's base): its answer (null when it has none), or the
    /// platform's error. `headers`: the call's own (Fly's lease nonce, Heroku's range).
    pub async fn call(&self, method: Method, path: &str, body: Option<&Value>, headers: &[(&str, &str)]) -> Result<Value> {
        let url = format!("{}{path}", base(self.kind));
        let mut req = client()?.request(method, &url).timeout(std::time::Duration::from_secs(60)).bearer(self.token).header("Accept", if self.kind == "heroku" { "application/vnd.heroku+json; version=3" } else { "application/json" });
        for (k, v) in headers {
            req = req.header(k, v);
        }
        if let Some(b) = body {
            req = req.json(b);
        }
        let resp = req.send().await.with_context(|| format!("{} {path}", self.kind))?;
        let status = resp.status();
        let bytes = resp.bytes(32 << 20).await?;
        if !(200..300).contains(&status) {
            return Err(error_of(status, &bytes).into());
        }
        Ok(serde_json::from_slice(&bytes).unwrap_or(Value::Null))
    }

    /// Sends a file's bytes: to `url` (a place the platform signed, `token: false`), or a path
    /// under the API's base.
    pub async fn upload(&self, method: Method, url: &str, content_type: &str, data: Vec<u8>, token: bool) -> Result<Value> {
        let url = if url.starts_with("http") { url.to_owned() } else { format!("{}{url}", base(self.kind)) };
        let mut req = client()?.request(method, &url).timeout(std::time::Duration::from_secs(300)).header("Content-Type", content_type).header("Content-Length", &data.len().to_string());
        if token {
            req = req.bearer(self.token);
        }
        let shown = url.split('?').next().unwrap_or(&url).to_owned();
        let resp = req.body(data).send().await.with_context(|| format!("uploading to {shown}"))?;
        let status = resp.status();
        let bytes = resp.bytes(1 << 20).await?;
        if !(200..300).contains(&status) {
            return Err(error_of(status, &bytes).into());
        }
        Ok(serde_json::from_slice(&bytes).unwrap_or(Value::Null))
    }
}

/// Whether a platform's token works: who it signs in as.
pub async fn verify(kind: &str, token: &str, config: &Value) -> Result<String> {
    let api = Api { kind, token };
    let s = |v: &Value, p: &str| v.pointer(p).and_then(Value::as_str).unwrap_or("someone").to_owned();
    Ok(match kind {
        "render" => {
            let owners = api.call(Method::Get, "/v1/owners?limit=1", None, &[]).await?;
            format!("signed in to Render's workspace {}", s(&owners, "/0/owner/name"))
        }
        "netlify" => format!("signed in to Netlify as {}", s(&api.call(Method::Get, "/api/v1/user", None, &[]).await?, "/email")),
        "heroku" => format!("signed in to Heroku as {}", s(&api.call(Method::Get, "/account", None, &[]).await?, "/email")),
        "digitalocean" => format!("signed in to DigitalOcean as {}", s(&api.call(Method::Get, "/v2/account", None, &[]).await?, "/account/email")),
        "fly" => {
            let org = config.get("org").and_then(Value::as_str).filter(|o| !o.is_empty()).unwrap_or("personal");
            let apps = api.call(Method::Get, &format!("/v1/apps?org_slug={}", super::aws::encode(org, false)), None, &[]).await?;
            format!("signed in to Fly.io: {} apps in {org}", apps.get("total_apps").and_then(Value::as_u64).unwrap_or(0))
        }
        other => anyhow::bail!("no check for {other}"),
    })
}
