//! Oracle Cloud (docs/ci.md W12-R03, Appendix D.2): a leg's ID token exchanged at an OCI IAM
//! identity domain for a user principal session token bound to a key made for it, then
//! Container Registry's token, asked for with a request that key signs. No SDK.
//!
//! - **The exchange** (RFC 8693, as identity domains take it): `POST <domain>/oauth2/v1/token`
//!   with the domain's confidential application's client ID and secret as basic credentials,
//!   `requested_token_type=urn:oci:token-type:oci-upst`, the ID token as a `jwt` subject token,
//!   and the new key's public half (its SubjectPublicKeyInfo, in base64). The domain trusts the
//!   issuer and audience it was set up with.
//! - **Container Registry**: `GET /20180419/docker/token` on the registry's host, signed as OCI
//!   signs requests (an HTTP signature over `(request-target)`, `host` and `date`, RSA-SHA256), its
//!   key ID `ST$<session token>`. The token it answers with logs in as `BEARER_TOKEN`.
//!
//! The key is made in memory for the one login and never written down.

use anyhow::{Context as _, Result, anyhow};
use aws_lc_rs::encoding::AsDer as _;
use aws_lc_rs::rand::SystemRandom;
use aws_lc_rs::rsa::{KeyPair, KeySize};
use aws_lc_rs::signature::{KeyPair as _, RSA_PKCS1_SHA256};
use base64::Engine as _;
use serde_json::Value;

use crate::http::client::Client;

/// The largest answer read from Oracle.
const LIMIT: usize = 1 << 20;

/// The username Container Registry's token logs in as.
pub const BEARER_USER: &str = "BEARER_TOKEN";

/// A key for a session: made here, its public half sent with the exchange.
pub struct SessionKey {
    key: KeyPair,
}

impl SessionKey {
    pub fn new() -> Result<SessionKey> {
        Ok(SessionKey { key: KeyPair::generate(KeySize::Rsa2048).map_err(|_| anyhow!("an RSA key couldn't be made"))? })
    }

    /// Its public half as the exchange takes it: its SubjectPublicKeyInfo's DER, in base64.
    pub fn public(&self) -> Result<String> {
        let der = self.key.public_key().as_der().map_err(|_| anyhow!("the key's public half couldn't be encoded"))?;
        Ok(base64::engine::general_purpose::STANDARD.encode(der.as_ref()))
    }

    /// An HTTP signature's `Authorization` for a request: `method` and `path` (with its query), to
    /// `host`, at `date`, as `key_id`.
    pub fn authorization(&self, key_id: &str, method: &str, path: &str, host: &str, date: &str) -> Result<String> {
        let text = signing_text(method, path, host, date);
        let mut sig = vec![0u8; self.key.public_modulus_len()];
        self.key.sign(&RSA_PKCS1_SHA256, &SystemRandom::new(), text.as_bytes(), &mut sig).map_err(|_| anyhow!("the request couldn't be signed"))?;
        let sig = base64::engine::general_purpose::STANDARD.encode(sig);
        Ok(format!("Signature version=\"1\",keyId=\"{key_id}\",algorithm=\"rsa-sha256\",headers=\"(request-target) host date\",signature=\"{sig}\""))
    }
}

/// What an HTTP signature signs, for its headers `(request-target) host date`.
pub fn signing_text(method: &str, path: &str, host: &str, date: &str) -> String {
    format!("(request-target): {} {path}\nhost: {host}\ndate: {date}", method.to_ascii_lowercase())
}

/// `application/x-www-form-urlencoded`'s encoding of `s`.
fn form(s: &str) -> String {
    super::alibaba::percent(s)
}

/// The identity domain's session token for the ID token `jwt`, bound to `public` (a
/// [`SessionKey`]'s).
pub async fn exchange(http: &Client, domain: &str, client_id: &str, client_secret: &str, jwt: &str, public: &str) -> Result<String> {
    let base = super::endpoint("oracle:identity", || domain.trim_end_matches('/').to_owned());
    let body = [
        ("grant_type", "urn:ietf:params:oauth:grant-type:token-exchange"),
        ("requested_token_type", "urn:oci:token-type:oci-upst"),
        ("subject_token", jwt),
        ("subject_token_type", "jwt"),
        ("public_key", public),
    ]
    .iter()
    .map(|(k, v)| format!("{k}={}", form(v)))
    .collect::<Vec<_>>()
    .join("&");
    let basic = base64::engine::general_purpose::STANDARD.encode(format!("{client_id}:{client_secret}"));
    let r = http
        .post(&format!("{base}/oauth2/v1/token"))
        .header("Authorization", &format!("Basic {basic}"))
        .header("Content-Type", "application/x-www-form-urlencoded;charset=utf-8")
        .body(body)
        .send()
        .await
        .context("the OCI identity domain's token exchange")?;
    if !r.is_success() {
        let status = r.status();
        let v: Value = r.json(LIMIT).await.unwrap_or(Value::Null);
        let said = v.get("error_description").or_else(|| v.get("error")).and_then(Value::as_str).unwrap_or_default().to_owned();
        anyhow::bail!("the OCI identity domain refused the exchange ({status}): {said}");
    }
    let v: Value = r.json(LIMIT).await?;
    v.get("token").or_else(|| v.get("access_token")).and_then(Value::as_str).map(str::to_owned).ok_or_else(|| anyhow!("the identity domain's answer has no token"))
}

/// Container Registry's token at `host` for the session `token`, its request signed by `key`.
pub async fn registry_token(http: &Client, host: &str, token: &str, key: &SessionKey) -> Result<String> {
    let base = super::endpoint("oracle:ocir", || format!("https://{host}"));
    let path = "/20180419/docker/token";
    let authority = base.split_once("://").map_or(base.as_str(), |(_, rest)| rest).split('/').next().unwrap_or(host).to_owned();
    let date = super::alibaba::http_date();
    let auth = key.authorization(&format!("ST${token}"), "GET", path, &authority, &date)?;
    let r = http.get(&format!("{base}{path}")).header("Date", &date).header("Authorization", &auth).send().await.context("Container Registry's docker token")?;
    anyhow::ensure!(r.is_success(), "Container Registry refused the session's request ({})", r.status());
    let v: Value = r.json(LIMIT).await?;
    v.get("token").or_else(|| v.get("access_token")).and_then(Value::as_str).map(str::to_owned).ok_or_else(|| anyhow!("Container Registry's answer has no token"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;
    use std::sync::Arc;

    use http::StatusCode;
    use parking_lot::Mutex;
    use serde_json::json;

    use crate::http::server::{self as srv, Request, Response};

    /// An identity domain and Container Registry as Oracle answers them: a session token for the
    /// client's exchange of its ID token, bound to the key sent, and a registry token for a
    /// request that key signed.
    #[derive(Default)]
    struct Oracle {
        public: Mutex<Option<Vec<u8>>>,
    }

    impl srv::Handler for Oracle {
        async fn handle(self: Arc<Self>, req: Request) -> Response {
            let (method, path) = (req.method().as_str().to_owned(), req.uri().path().to_owned());
            let header = |h: &str| req.headers().get(h).and_then(|v| v.to_str().ok()).unwrap_or_default().to_owned();
            let (auth, host, date) = (header("authorization"), header("host"), header("date"));
            let body = srv::body_bytes(req.into_body(), 1 << 20).await.unwrap_or_default();
            let no = |m: &str| srv::json(StatusCode::UNAUTHORIZED, &json!({ "error": "unauthorized", "error_description": m }));
            match (method.as_str(), path.as_str()) {
                ("POST", "/oauth2/v1/token") => {
                    let form: HashMap<String, String> = String::from_utf8_lossy(&body).split('&').filter_map(|kv| kv.split_once('=')).map(|(k, v)| (k.to_owned(), srv::decode(v))).collect();
                    let get = |k: &str| form.get(k).map(String::as_str).unwrap_or_default();
                    let basic = format!("Basic {}", base64::engine::general_purpose::STANDARD.encode("cid:csecret"));
                    if auth != basic
                        || (get("grant_type"), get("requested_token_type"), get("subject_token"), get("subject_token_type"))
                            != ("urn:ietf:params:oauth:grant-type:token-exchange", "urn:oci:token-type:oci-upst", "jwt-2", "jwt")
                    {
                        return no(&format!("{auth} {form:?}"));
                    }
                    *self.public.lock() = base64::engine::general_purpose::STANDARD.decode(get("public_key")).ok();
                    srv::json(StatusCode::OK, &json!({ "token": "upst-1" }))
                }
                ("GET", "/20180419/docker/token") => {
                    let Some(public) = self.public.lock().clone() else { return no("no session") };
                    let field = |k: &str| auth.split(&format!("{k}=\"")).nth(1).and_then(|r| r.split('"').next()).unwrap_or_default().to_owned();
                    let sig = base64::engine::general_purpose::STANDARD.decode(field("signature")).unwrap_or_default();
                    let text = signing_text("GET", &path, &host, &date);
                    let verified = aws_lc_rs::signature::UnparsedPublicKey::new(&aws_lc_rs::signature::RSA_PKCS1_2048_8192_SHA256, &public).verify(text.as_bytes(), &sig).is_ok();
                    if field("keyId") != "ST$upst-1" || field("headers") != "(request-target) host date" || !verified {
                        return no(&format!("{auth} over {text:?}"));
                    }
                    srv::json(StatusCode::OK, &json!({ "token": "ocir-token" }))
                }
                _ => no(&format!("nothing at {method} {path}")),
            }
        }
    }

    /// W12-R03, D.2: the leg's ID token exchanged for a session bound to a key made for it, and
    /// Container Registry's token for a request that key signed; a session the domain refused
    /// gets nothing.
    #[test]
    fn w12_r03_oracle_container_registry_through_a_session() -> anyhow::Result<()> {
        crate::rt::block_on(async {
            let base = crate::ci::integrations::alibaba::tests::serve(Arc::new(Oracle::default()))?;
            crate::ci::integrations::override_endpoint("oracle:identity", &base);
            crate::ci::integrations::override_endpoint("oracle:ocir", &base);
            let http = crate::http::client::Client::public()?;
            let key = SessionKey::new()?;
            let upst = exchange(&http, "https://idcs-1.identity.oraclecloud.com", "cid", "csecret", "jwt-2", &key.public()?).await?;
            assert_eq!(upst, "upst-1");
            assert_eq!(registry_token(&http, "iad.ocir.io", &upst, &key).await?, "ocir-token");
            // Another key than the session's doesn't sign for it.
            let other = SessionKey::new()?;
            assert!(registry_token(&http, "iad.ocir.io", &upst, &other).await.is_err());
            let e = exchange(&http, "https://idcs-1.identity.oraclecloud.com", "cid", "wrong", "jwt-2", &key.public()?).await.err().map(|e| format!("{e:#}")).unwrap_or_default();
            assert!(e.contains("refused the exchange (401)"), "{e}");
            Ok(())
        })
    }

    #[test]
    fn w12_r03_a_signed_request_verifies_with_the_key_sent() -> anyhow::Result<()> {
        let key = SessionKey::new()?;
        let public = base64::engine::general_purpose::STANDARD.decode(key.public()?)?;
        let date = "Wed, 21 Oct 2026 09:00:00 GMT";
        let auth = key.authorization("ST$upst", "GET", "/20180419/docker/token", "iad.ocir.io", date)?;
        assert!(auth.starts_with("Signature version=\"1\",keyId=\"ST$upst\",algorithm=\"rsa-sha256\",headers=\"(request-target) host date\",signature=\""), "{auth}");
        let sig = auth.rsplit_once("signature=\"").map(|(_, s)| s.trim_end_matches('"')).unwrap_or_default();
        let sig = base64::engine::general_purpose::STANDARD.decode(sig)?;
        let text = signing_text("GET", "/20180419/docker/token", "iad.ocir.io", date);
        assert_eq!(text, "(request-target): get /20180419/docker/token\nhost: iad.ocir.io\ndate: Wed, 21 Oct 2026 09:00:00 GMT");
        let verifier = aws_lc_rs::signature::UnparsedPublicKey::new(&aws_lc_rs::signature::RSA_PKCS1_2048_8192_SHA256, &public);
        verifier.verify(text.as_bytes(), &sig).map_err(|_| anyhow!("the signature doesn't verify with the key the exchange was sent"))?;
        assert!(verifier.verify(b"something else", &sig).is_err());
        Ok(())
    }
}
