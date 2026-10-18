//! Google Cloud (docs/ci.md W11; docs/cd-targets.md §6): kiln's ID token for a workload,
//! exchanged at Google's Security Token Service for a federated access token (workload identity
//! federation), then, when the integration names a service account, for that account's token
//! (`generateAccessToken`). No key is stored. Tokens are kept in memory until two minutes before
//! they expire, and are never written down or printed.

use std::collections::HashMap;
use std::sync::OnceLock;

use anyhow::{Context as _, Result, bail};
use parking_lot::Mutex;
use serde_json::{Value, json};

use super::Integration;
use crate::hub::Hub;
use crate::http::client::{Client, Roots};

/// The scope every Google Cloud API accepts.
pub const CLOUD_PLATFORM: &str = "https://www.googleapis.com/auth/cloud-platform";

/// How long an impersonated token is asked for.
const LIFETIME_SECS: u32 = 900;

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

/// An error Google answered: its status and message (`PERMISSION_DENIED: …`, `invalid_grant: …`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GcpError {
    pub status: u16,
    pub code: String,
    pub message: String,
}

impl std::fmt::Display for GcpError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}: {} (HTTP {})", self.code, self.message, self.status)
    }
}

impl std::error::Error for GcpError {}

impl GcpError {
    /// Whether `e` is Google saying something doesn't exist.
    pub fn is_not_found(e: &anyhow::Error) -> bool {
        e.downcast_ref::<GcpError>().is_some_and(|g| g.status == 404 || g.code == "NOT_FOUND")
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

/// Google's error shapes: OAuth's (`{error, error_description}`) and the APIs'
/// (`{error: {code, message, status}}`).
pub fn error_of(status: u16, body: &[u8]) -> GcpError {
    let v: Value = serde_json::from_slice(body).unwrap_or(Value::Null);
    let (code, message) = match v.get("error") {
        Some(Value::String(e)) => (e.clone(), v.get("error_description").and_then(Value::as_str).unwrap_or_default().to_owned()),
        Some(Value::Object(o)) => (
            o.get("status").and_then(Value::as_str).unwrap_or("ERROR").to_owned(),
            o.get("message").and_then(Value::as_str).unwrap_or_default().to_owned(),
        ),
        _ => ("ERROR".to_owned(), String::from_utf8_lossy(body).chars().take(300).collect()),
    };
    GcpError { status, code, message }
}

/// Exchanges a kiln ID token for a federated access token. `provider` is the workload identity
/// provider's resource name (`projects/<number>/locations/global/workloadIdentityPools/<pool>/providers/<provider>`).
pub async fn exchange(provider: &str, id_token: &str) -> Result<Token> {
    let base = super::endpoint("gcp:sts", || "https://sts.googleapis.com".into());
    // OAuth 2.0 token exchange (RFC 8693), as Google's own libraries send it.
    let form = [
        ("grant_type", "urn:ietf:params:oauth:grant-type:token-exchange".to_owned()),
        ("audience", format!("//iam.googleapis.com/{provider}")),
        ("scope", CLOUD_PLATFORM.to_owned()),
        ("requested_token_type", "urn:ietf:params:oauth:token-type:access_token".to_owned()),
        ("subject_token", id_token.to_owned()),
        ("subject_token_type", "urn:ietf:params:oauth:token-type:jwt".to_owned()),
    ];
    let body = form.iter().map(|(k, v)| format!("{k}={}", super::aws::encode(v, false))).collect::<Vec<_>>().join("&");
    let resp = client()?.post(&format!("{base}/v1/token")).timeout(std::time::Duration::from_secs(30)).header("content-type", "application/x-www-form-urlencoded").body(body).send().await.context("Google's Security Token Service")?;
    let status = resp.status();
    let bytes = resp.bytes(1 << 20).await?;
    if !(200..300).contains(&status) {
        return Err(error_of(status, &bytes).into());
    }
    let v: Value = serde_json::from_slice(&bytes).context("Google's STS answered something that isn't JSON")?;
    let token = v.get("access_token").and_then(Value::as_str).ok_or_else(|| anyhow::anyhow!("Google's STS answered without a token"))?;
    let expires_in = v.get("expires_in").and_then(Value::as_i64).unwrap_or(3600);
    Ok(Token { access_token: token.to_owned(), expires_at: crate::util::now_unix() + expires_in })
}

/// A service account's access token, asked for with a federated token allowed to impersonate it.
pub async fn impersonate(service_account: &str, federated: &str) -> Result<Token> {
    let base = super::endpoint("gcp:iamcredentials", || "https://iamcredentials.googleapis.com".into());
    if !service_account.bytes().all(|b| b.is_ascii_alphanumeric() || matches!(b, b'@' | b'.' | b'-' | b'_')) {
        bail!("`{service_account}` is not a service account's email");
    }
    let url = format!("{base}/v1/projects/-/serviceAccounts/{service_account}:generateAccessToken");
    let body = json!({ "scope": [CLOUD_PLATFORM], "lifetime": format!("{LIFETIME_SECS}s") });
    let resp = client()?.post(&url).timeout(std::time::Duration::from_secs(30)).bearer(federated).json(&body).send().await.context("Google's IAM credentials API")?;
    let status = resp.status();
    let bytes = resp.bytes(1 << 20).await?;
    if !(200..300).contains(&status) {
        return Err(error_of(status, &bytes).into());
    }
    let v: Value = serde_json::from_slice(&bytes).context("Google's IAM credentials API answered something that isn't JSON")?;
    let token = v.get("accessToken").and_then(Value::as_str).ok_or_else(|| anyhow::anyhow!("Google answered without a token"))?;
    let expires_at = v
        .get("expireTime")
        .and_then(Value::as_str)
        .and_then(|e| time::OffsetDateTime::parse(e, &time::format_description::well_known::Rfc3339).ok())
        .map_or(crate::util::now_unix() + i64::from(LIFETIME_SECS), |t| t.unix_timestamp());
    Ok(Token { access_token: token.to_owned(), expires_at })
}

fn minted() -> &'static Mutex<HashMap<String, Token>> {
    static M: OnceLock<Mutex<HashMap<String, Token>>> = OnceLock::new();
    M.get_or_init(|| Mutex::new(HashMap::new()))
}

/// A `gcp` integration's access token for `w` (W11-R03): the provider's federated token, or the
/// service account's when the integration names one. Kept until two minutes before it expires.
pub async fn token(hub: &Hub, i: &Integration, w: &kiln_ci::oidc::Workload<'_>) -> Result<Token> {
    let cfg = |k: &str| i.config.get(k).and_then(Value::as_str).map(str::trim).filter(|s| !s.is_empty()).map(str::to_owned);
    let Some(provider) = cfg("provider") else { bail!("{} has no provider", i.name) };
    let provider = provider.trim_start_matches("//iam.googleapis.com/").trim_start_matches('/').to_owned();
    let account = cfg("service_account");
    let key = format!("{}|{}|{}|{}|{}|{}", i.id, w.repository, w.environment.unwrap_or(""), w.app.unwrap_or(""), w.trigger.unwrap_or(""), account.as_deref().unwrap_or(""));
    let now = crate::util::now_unix();
    if let Some(t) = minted().lock().get(&key).filter(|t| t.expires_at - 120 > now) {
        return Ok(t.clone());
    }
    // The provider's default allowed audience.
    let audience = format!("https://iam.googleapis.com/{provider}");
    let issuer = crate::ci::oidc::issuer(hub);
    let claims = kiln_ci::oidc::workload_claims(w, &issuer, &audience, &crate::util::random_token(), now, 300, None);
    let id_token = crate::ci::oidc::mint(hub, &claims).await?;
    let federated = exchange(&provider, &id_token).await.with_context(|| format!("exchanging kiln's token at {provider}"))?;
    let t = match account {
        Some(a) => impersonate(&a, &federated.access_token).await.with_context(|| format!("impersonating {a}"))?,
        None => federated,
    };
    minted().lock().insert(key, t.clone());
    Ok(t)
}

/// What to check when Google refuses kiln's token.
pub fn advice(e: &anyhow::Error) -> Option<&'static str> {
    let g = e.downcast_ref::<GcpError>()?;
    Some(match g.code.as_str() {
        "invalid_grant" if g.message.contains("audience") => "the workload identity provider doesn't accept this audience: leave its allowed audiences at the default",
        "invalid_grant" => "the workload identity provider refused kiln's token: check its issuer URL and its attribute condition",
        "invalid_target" => "no such workload identity provider: check the provider's resource name",
        "PERMISSION_DENIED" => "the federated identity may not impersonate the service account: grant it roles/iam.workloadIdentityUser on it",
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn errors_read_in_both_shapes() {
        let o = error_of(400, br#"{"error":"invalid_grant","error_description":"The audience in ID Token does not match"}"#);
        assert_eq!((o.code.as_str(), o.message.as_str()), ("invalid_grant", "The audience in ID Token does not match"));
        assert!(advice(&anyhow::Error::new(o)).is_some_and(|a| a.contains("audience")));
        let a = error_of(403, br#"{"error":{"code":403,"message":"Permission 'iam.serviceAccounts.getAccessToken' denied","status":"PERMISSION_DENIED"}}"#);
        assert_eq!(a.code, "PERMISSION_DENIED");
        assert!(GcpError::is_not_found(&anyhow::Error::new(error_of(404, br#"{"error":{"code":404,"message":"no","status":"NOT_FOUND"}}"#))));
    }

    /// Google's STS and IAM credentials API, faked: the federated token impersonates the service
    /// account.
    #[test]
    fn tokens_exchange_then_impersonate() -> anyhow::Result<()> {
        use std::sync::Arc;

        use crate::http::server::{self as srv, Request, Response};

        struct Fake;
        impl srv::Handler for Fake {
            async fn handle(self: Arc<Self>, req: Request) -> Response {
                let path = req.uri().path().to_owned();
                let auth = srv::header_str(&req, &http::header::AUTHORIZATION).unwrap_or("").to_owned();
                let body = srv::body_bytes(req.into_body(), 1 << 20).await.unwrap_or_default();
                let form: HashMap<String, String> = String::from_utf8_lossy(&body).split('&').filter_map(|kv| kv.split_once('=')).map(|(k, v)| (k.to_owned(), srv::decode(v))).collect();
                match path.as_str() {
                    "/v1/token" => {
                        let aud = form.get("audience").map(String::as_str).unwrap_or_default();
                        if form.get("subject_token").map(String::as_str) != Some("idtok") || aud != "//iam.googleapis.com/projects/1/locations/global/workloadIdentityPools/p/providers/kiln" || form.get("grant_type").map(String::as_str) != Some("urn:ietf:params:oauth:grant-type:token-exchange") {
                            return srv::json(http::StatusCode::BAD_REQUEST, &json!({ "error": "invalid_grant", "error_description": "bad token" }));
                        }
                        srv::json(http::StatusCode::OK, &json!({ "access_token": "fed", "expires_in": 3600, "token_type": "Bearer" }))
                    }
                    "/v1/projects/-/serviceAccounts/deploy@proj.iam.gserviceaccount.com:generateAccessToken" if auth == "Bearer fed" => {
                        srv::json(http::StatusCode::OK, &json!({ "accessToken": "sa-token", "expireTime": "2030-01-01T00:00:00Z" }))
                    }
                    _ => srv::json(http::StatusCode::FORBIDDEN, &json!({ "error": { "code": 403, "message": format!("denied {path}"), "status": "PERMISSION_DENIED" } })),
                }
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
            let base = format!("http://{addr}");
            super::super::override_endpoint("gcp:sts", &base);
            super::super::override_endpoint("gcp:iamcredentials", &base);
            let provider = "projects/1/locations/global/workloadIdentityPools/p/providers/kiln";
            let bad = exchange(provider, "nope").await.err().ok_or_else(|| anyhow::anyhow!("a bad token was exchanged"))?;
            assert!(advice(&bad).is_some());
            let fed = exchange(provider, "idtok").await?;
            assert_eq!(fed.access_token, "fed");
            let sa = impersonate("deploy@proj.iam.gserviceaccount.com", &fed.access_token).await?;
            assert_eq!((sa.access_token.as_str(), sa.expires_at), ("sa-token", 1_893_456_000));
            assert!(!format!("{sa:?}").contains("sa-token"));
            let denied = impersonate("other@proj.iam.gserviceaccount.com", &fed.access_token).await.err().ok_or_else(|| anyhow::anyhow!("impersonated another account"))?;
            assert_eq!(denied.downcast_ref::<GcpError>().map(|g| g.code.as_str()), Some("PERMISSION_DENIED"));
            Ok(())
        })
    }
}
