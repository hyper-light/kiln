//! Azure (docs/ci.md W11; docs/cd-targets.md §6): kiln's ID token for a workload, given to Entra
//! ID as a client assertion for the app registration that trusts it (a federated identity
//! credential), for an access token to one resource (Azure Resource Manager, Storage). No secret
//! is stored. Tokens are kept in memory until two minutes before they expire, and are never
//! written down or printed.

use std::collections::HashMap;
use std::sync::OnceLock;

use anyhow::{Context as _, Result, bail};
use parking_lot::Mutex;
use serde_json::Value;

use super::Integration;
use crate::hub::Hub;
use crate::http::client::{Client, Roots};

/// Azure Resource Manager's scope.
pub const MANAGEMENT: &str = "https://management.azure.com/.default";

/// The audience Entra expects of a federated credential's token.
pub const AUDIENCE: &str = "api://AzureADTokenExchange";

/// An access token, until it expires.
#[derive(Clone, PartialEq, Eq)]
pub struct Token {
    pub access_token: String,
    /// Unix seconds.
    pub expires_at: i64,
}

impl std::fmt::Debug for Token {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Token").field("access_token", &"<redacted>").field("expires_at", &self.expires_at).finish()
    }
}

/// An error Azure answered: Entra's (`invalid_client`, with its `AADSTS` code in the message), or
/// Resource Manager's (`{error: {code, message}}`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AzureError {
    pub status: u16,
    pub code: String,
    pub message: String,
}

impl std::fmt::Display for AzureError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}: {} (HTTP {})", self.code, self.message, self.status)
    }
}

impl std::error::Error for AzureError {}

impl AzureError {
    /// Whether `e` is Azure saying something doesn't exist.
    pub fn is_not_found(e: &anyhow::Error) -> bool {
        e.downcast_ref::<AzureError>().is_some_and(|a| a.status == 404 || a.code.ends_with("NotFound"))
    }
}

/// Azure's error shapes.
pub fn error_of(status: u16, body: &[u8]) -> AzureError {
    let v: Value = serde_json::from_slice(body).unwrap_or(Value::Null);
    let (code, message) = match v.get("error") {
        Some(Value::String(e)) => (e.clone(), v.get("error_description").and_then(Value::as_str).unwrap_or_default().lines().next().unwrap_or_default().to_owned()),
        Some(Value::Object(o)) => (o.get("code").and_then(Value::as_str).unwrap_or("Error").to_owned(), o.get("message").and_then(Value::as_str).unwrap_or_default().to_owned()),
        _ => ("Error".to_owned(), String::from_utf8_lossy(body).chars().take(300).collect()),
    };
    AzureError { status, code, message }
}

fn client() -> Result<&'static Client> {
    static C: OnceLock<Client> = OnceLock::new();
    if let Some(c) = C.get() {
        return Ok(c);
    }
    let c = Client::new(Roots::WebPki, None)?;
    Ok(C.get_or_init(|| c))
}

/// Asks Entra for `scope`'s token as `client_id` in `tenant`, with kiln's ID token as the client
/// assertion.
pub async fn exchange(tenant: &str, client_id: &str, scope: &str, assertion: &str) -> Result<Token> {
    if !tenant.bytes().all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'.')) {
        bail!("`{tenant}` is not a tenant");
    }
    let base = super::endpoint("azure:login", || "https://login.microsoftonline.com".into());
    let form = [
        ("client_id", client_id),
        ("scope", scope),
        ("client_assertion_type", "urn:ietf:params:oauth:client-assertion-type:jwt-bearer"),
        ("client_assertion", assertion),
        ("grant_type", "client_credentials"),
    ];
    let body = form.iter().map(|(k, v)| format!("{k}={}", super::aws::encode(v, false))).collect::<Vec<_>>().join("&");
    let resp = client()?
        .post(&format!("{base}/{tenant}/oauth2/v2.0/token"))
        .timeout(std::time::Duration::from_secs(30))
        .header("content-type", "application/x-www-form-urlencoded")
        .body(body)
        .send()
        .await
        .context("Entra ID")?;
    let status = resp.status();
    let bytes = resp.bytes(1 << 20).await?;
    if !(200..300).contains(&status) {
        return Err(error_of(status, &bytes).into());
    }
    let v: Value = serde_json::from_slice(&bytes).context("Entra answered something that isn't JSON")?;
    let token = v.get("access_token").and_then(Value::as_str).ok_or_else(|| anyhow::anyhow!("Entra answered without a token"))?;
    let expires_in = v.get("expires_in").and_then(|e| e.as_i64().or_else(|| e.as_str().and_then(|s| s.parse().ok()))).unwrap_or(3600);
    Ok(Token { access_token: token.to_owned(), expires_at: crate::util::now_unix() + expires_in })
}

fn minted() -> &'static Mutex<HashMap<String, Token>> {
    static M: OnceLock<Mutex<HashMap<String, Token>>> = OnceLock::new();
    M.get_or_init(|| Mutex::new(HashMap::new()))
}

/// An `azure` integration's token for `scope` (Resource Manager's, by default) for `w` (W11-R03).
/// Kept until two minutes before it expires.
pub async fn token(hub: &Hub, i: &Integration, w: &kiln_ci::oidc::Workload<'_>, scope: Option<&str>) -> Result<Token> {
    let cfg = |k: &str| i.config.get(k).and_then(Value::as_str).map(str::trim).filter(|s| !s.is_empty()).map(str::to_owned);
    let (Some(tenant), Some(client_id)) = (cfg("tenant_id"), cfg("client_id")) else { bail!("{} needs tenant_id and client_id", i.name) };
    let scope = scope.unwrap_or(MANAGEMENT);
    let key = format!("{}|{}|{}|{}|{}|{scope}", i.id, w.repository, w.environment.unwrap_or(""), w.app.unwrap_or(""), w.trigger.unwrap_or(""));
    let now = crate::util::now_unix();
    if let Some(t) = minted().lock().get(&key).filter(|t| t.expires_at - 120 > now) {
        return Ok(t.clone());
    }
    let issuer = crate::ci::oidc::issuer(hub);
    let claims = kiln_ci::oidc::workload_claims(w, &issuer, AUDIENCE, &crate::util::random_token(), now, 300, None);
    let assertion = crate::ci::oidc::mint(hub, &claims).await?;
    let t = exchange(&tenant, &client_id, scope, &assertion).await.with_context(|| format!("signing in as {client_id} in {tenant}"))?;
    minted().lock().insert(key, t.clone());
    Ok(t)
}

/// What to check when Entra refuses kiln's token, by its `AADSTS` code.
pub fn advice(e: &anyhow::Error) -> Option<&'static str> {
    let a = e.downcast_ref::<AzureError>()?;
    let m = a.message.as_str();
    Some(if m.contains("AADSTS70021") {
        "no federated credential on the app registration matches this token: add one with kiln's issuer, this subject, and the audience api://AzureADTokenExchange"
    } else if m.contains("AADSTS700211") || m.contains("AADSTS700212") {
        "Entra can't check kiln's token: the federated credential's issuer must be kiln's issuer URL, and Entra must reach its keys (a public URL, or `kiln oidc publish`)"
    } else if m.contains("AADSTS700016") {
        "no such application in this tenant: check client_id and tenant_id"
    } else if m.contains("AADSTS90002") {
        "no such tenant: check tenant_id"
    } else {
        return None;
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn errors_read_in_both_shapes() {
        let e = error_of(400, br#"{"error":"invalid_client","error_description":"AADSTS70021: No matching federated identity record found for presented assertion.\r\nTrace ID: 1"}"#);
        assert_eq!(e.code, "invalid_client");
        assert!(!e.message.contains("Trace ID"));
        assert!(advice(&anyhow::Error::new(e)).is_some_and(|a| a.contains("federated credential")));
        let arm = error_of(404, br#"{"error":{"code":"ResourceGroupNotFound","message":"Resource group 'rg' could not be found."}}"#);
        assert!(AzureError::is_not_found(&anyhow::Error::new(arm)));
    }

    /// Entra, faked: the assertion signs in as the app registration.
    #[test]
    fn assertions_sign_in() -> anyhow::Result<()> {
        use std::sync::Arc;

        use crate::http::server::{self as srv, Request, Response};

        struct Fake;
        impl srv::Handler for Fake {
            async fn handle(self: Arc<Self>, req: Request) -> Response {
                let path = req.uri().path().to_owned();
                let body = srv::body_bytes(req.into_body(), 1 << 20).await.unwrap_or_default();
                let form: HashMap<String, String> = String::from_utf8_lossy(&body).split('&').filter_map(|kv| kv.split_once('=')).map(|(k, v)| (k.to_owned(), srv::decode(v))).collect();
                let get = |k: &str| form.get(k).map(String::as_str).unwrap_or_default();
                if path != "/tenant-1/oauth2/v2.0/token" || get("client_id") != "app-1" || get("grant_type") != "client_credentials" {
                    return srv::json(http::StatusCode::BAD_REQUEST, &serde_json::json!({ "error": "invalid_request", "error_description": "AADSTS90002: Tenant not found." }));
                }
                if get("client_assertion") != "idtok" || get("client_assertion_type") != "urn:ietf:params:oauth:client-assertion-type:jwt-bearer" {
                    return srv::json(http::StatusCode::BAD_REQUEST, &serde_json::json!({ "error": "invalid_client", "error_description": "AADSTS70021: No matching federated identity record found." }));
                }
                srv::json(http::StatusCode::OK, &serde_json::json!({ "token_type": "Bearer", "expires_in": 3599, "access_token": format!("arm:{}", get("scope")) }))
            }
        }

        crate::rt::block_on(async {
            let tcp = hyper_rt::tcp::TcpListener::bind(std::net::SocketAddr::from(([127, 0, 0, 1], 0)), 16).map_err(|e| anyhow::anyhow!("{e}"))?;
            let addr = tcp.local_addr().map_err(|e| anyhow::anyhow!("{e}"))?;
            crate::rt::spawn(async move {
                let fake = Arc::new(Fake);
                while let Ok(s) = tcp.accept().await {
                    let (h, r) = (Arc::clone(&fake), s.peer_addr().ok());
                    crate::rt::spawn(srv::serve_connection(crate::http::io::RtIo::new(s), h, r));
                }
            });
            super::super::override_endpoint("azure:login", &format!("http://{addr}"));
            let t = exchange("tenant-1", "app-1", MANAGEMENT, "idtok").await?;
            assert_eq!(t.access_token, format!("arm:{MANAGEMENT}"));
            assert!(t.expires_at > crate::util::now_unix() + 3000);
            let refused = exchange("tenant-1", "app-1", MANAGEMENT, "other").await.err().ok_or_else(|| anyhow::anyhow!("a wrong assertion signed in"))?;
            assert!(advice(&refused).is_some_and(|a| a.contains("federated credential")));
            assert!(exchange("bad tenant", "app-1", MANAGEMENT, "idtok").await.is_err());
            Ok(())
        })
    }
}
