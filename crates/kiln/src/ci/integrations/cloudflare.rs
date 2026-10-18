//! Cloudflare (docs/ci.md W11; docs/cd-targets.md §6): its v4 API with an API token, which
//! kiln keeps in its secret store. Answers are `{success, errors, result}`; errors keep
//! Cloudflare's code and message. Uploads are multipart forms, as the Workers API takes them.

use std::sync::OnceLock;

use anyhow::{Context as _, Result};
use serde_json::Value;

use crate::http::client::{Client, Method, Roots};

/// An error Cloudflare answered.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CfError {
    pub status: u16,
    pub code: i64,
    pub message: String,
}

impl std::fmt::Display for CfError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Cloudflare {}: {} (HTTP {})", self.code, self.message, self.status)
    }
}

impl std::error::Error for CfError {}

impl CfError {
    /// Whether `e` is Cloudflare saying something doesn't exist.
    pub fn is_not_found(e: &anyhow::Error) -> bool {
        e.downcast_ref::<CfError>().is_some_and(|c| c.status == 404 || c.code == 10007 || c.code == 10090)
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

/// The API's base (`https://api.cloudflare.com/client/v4`), or a test's fake.
pub fn base() -> String {
    super::endpoint("cloudflare", || "https://api.cloudflare.com/client/v4".into())
}

/// One part of a multipart form.
pub struct Part {
    pub name: String,
    pub filename: Option<String>,
    pub content_type: String,
    pub data: Vec<u8>,
}

/// A multipart form's body, and its content type.
pub fn multipart(parts: &[Part]) -> (Vec<u8>, String) {
    let boundary = format!("kiln-{}", crate::util::random_token().replace(['-', '_'], ""));
    let mut body = Vec::new();
    for p in parts {
        body.extend_from_slice(format!("--{boundary}\r\n").as_bytes());
        let quote = |s: &str| s.replace('"', "%22").replace(['\r', '\n'], "");
        match &p.filename {
            Some(f) => body.extend_from_slice(format!("Content-Disposition: form-data; name=\"{}\"; filename=\"{}\"\r\n", quote(&p.name), quote(f)).as_bytes()),
            None => body.extend_from_slice(format!("Content-Disposition: form-data; name=\"{}\"\r\n", quote(&p.name)).as_bytes()),
        }
        body.extend_from_slice(format!("Content-Type: {}\r\n\r\n", p.content_type).as_bytes());
        body.extend_from_slice(&p.data);
        body.extend_from_slice(b"\r\n");
    }
    body.extend_from_slice(format!("--{boundary}--\r\n").as_bytes());
    (body, format!("multipart/form-data; boundary={boundary}"))
}

/// Cloudflare's API, with a token.
pub struct Cf<'a> {
    pub token: &'a str,
}

impl Cf<'_> {
    /// A call: its `result`, or the error Cloudflare gave.
    pub async fn call(&self, method: Method, path: &str, body: Option<&Value>) -> Result<Value> {
        let mut req = client()?.request(method, &format!("{}{path}", base())).timeout(std::time::Duration::from_secs(60)).bearer(self.token);
        if let Some(b) = body {
            req = req.json(b);
        }
        answer(req.send().await.with_context(|| format!("Cloudflare {path}"))?).await
    }

    /// An upload: a multipart form.
    pub async fn upload(&self, method: Method, path: &str, parts: &[Part]) -> Result<Value> {
        let (body, content_type) = multipart(parts);
        let req = client()?.request(method, &format!("{}{path}", base())).timeout(std::time::Duration::from_secs(300)).bearer(self.token).header("Content-Type", &content_type).body(body);
        answer(req.send().await.with_context(|| format!("Cloudflare {path}"))?).await
    }
}

async fn answer(resp: crate::http::client::Response) -> Result<Value> {
    let status = resp.status();
    let bytes = resp.bytes(32 << 20).await?;
    let v: Value = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
    let ok = v.get("success").and_then(Value::as_bool).unwrap_or((200..300).contains(&status));
    if ok && (200..300).contains(&status) {
        return Ok(v.get("result").cloned().unwrap_or(Value::Null));
    }
    let first = v.get("errors").and_then(Value::as_array).and_then(|e| e.first()).cloned().unwrap_or(Value::Null);
    let message = first.get("message").and_then(Value::as_str).map(str::to_owned).unwrap_or_else(|| String::from_utf8_lossy(&bytes).chars().take(300).collect());
    Err(CfError { status, code: first.get("code").and_then(Value::as_i64).unwrap_or(0), message }.into())
}

/// Whether a token works: an account's token checks against its account, a user's against the
/// user.
pub async fn verify(token: &str, account: Option<&str>) -> Result<String> {
    let cf = Cf { token };
    let r = match account {
        Some(a) => match cf.call(Method::Get, &format!("/accounts/{a}/tokens/verify"), None).await {
            Ok(r) => r,
            Err(_) => cf.call(Method::Get, "/user/tokens/verify", None).await?,
        },
        None => cf.call(Method::Get, "/user/tokens/verify", None).await?,
    };
    match r.get("status").and_then(Value::as_str) {
        Some("active") => Ok(format!("the token is active{}", account.map(|a| format!(" for account {a}")).unwrap_or_default())),
        other => anyhow::bail!("the token is {}", other.unwrap_or("not active")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn forms_are_multipart() {
        let (body, ct) = multipart(&[
            Part { name: "metadata".into(), filename: None, content_type: "application/json".into(), data: br#"{"main_module":"index.js"}"#.to_vec() },
            Part { name: "index.js".into(), filename: Some("index.js".into()), content_type: "application/javascript+module".into(), data: b"export default {}".to_vec() },
        ]);
        let boundary = ct.strip_prefix("multipart/form-data; boundary=").unwrap_or_default().to_owned();
        let text = String::from_utf8_lossy(&body);
        assert!(text.starts_with(&format!("--{boundary}\r\nContent-Disposition: form-data; name=\"metadata\"\r\n")));
        assert!(text.contains("name=\"index.js\"; filename=\"index.js\"\r\nContent-Type: application/javascript+module\r\n\r\nexport default {}\r\n"));
        assert!(text.ends_with(&format!("--{boundary}--\r\n")));
    }
}
