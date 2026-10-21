//! Alibaba Cloud (docs/ci.md W12-R03, Appendix D.2): a leg's ID token for a RAM role, then
//! Container Registry's temporary password, for `docker-credential-kiln`. No SDK: requests are made
//! and signed here as Alibaba's APIs want them.
//!
//! - **STS** (`AssumeRoleWithOIDC`): the ID token, the OIDC provider's and the role's ARNs. It
//!   needs no key, so it's sent unsigned.
//! - **Container Registry**: an Enterprise Edition instance's `GetAuthorizationToken` (RPC style,
//!   `2018-12-01`, with its `InstanceId`), or the Personal Edition's (ROA style, `2016-06-07`,
//!   `GET /tokens`), signed with the role's temporary key (HMAC-SHA1, signature version 1.0) and
//!   its security token. Its answer is a username (`cr_temp_user`) and a password.
//!
//! What's made lives in the credential helper's process only, and lasts an hour at most.

use std::collections::BTreeMap;

use anyhow::{Context as _, Result, anyhow};
use base64::Engine as _;
use serde_json::Value;

use crate::http::client::Client;

/// The largest answer read from Alibaba.
const LIMIT: usize = 1 << 20;

/// A RAM role's temporary credentials.
#[derive(Clone, PartialEq, Eq)]
pub struct Credentials {
    pub access_key_id: String,
    pub access_key_secret: String,
    pub security_token: String,
}

impl std::fmt::Debug for Credentials {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Credentials").field("access_key_id", &self.access_key_id).finish_non_exhaustive()
    }
}

/// `s` as Alibaba's signatures encode it: RFC 3986's unreserved characters as they are, every
/// other byte as `%XX` (so a space is `%20`, `*` is `%2A`, and `~` stays).
pub fn percent(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.' | b'~') {
            out.push(char::from(b));
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

/// HMAC-SHA1 of `text` with `key`, in base64: both signature styles' signature.
fn hmac_sha1(key: &str, text: &str) -> String {
    let k = aws_lc_rs::hmac::Key::new(aws_lc_rs::hmac::HMAC_SHA1_FOR_LEGACY_USE_ONLY, key.as_bytes());
    base64::engine::general_purpose::STANDARD.encode(aws_lc_rs::hmac::sign(&k, text.as_bytes()).as_ref())
}

/// An RPC-style request's query, `params` (the common ones among them) and its `Signature`:
/// the parameters sorted and encoded, signed as `GET&%2F&<them, encoded again>` with the secret
/// and an `&`.
pub fn rpc_query(method: &str, params: &BTreeMap<String, String>, secret: &str) -> String {
    let canonical = params.iter().map(|(k, v)| format!("{}={}", percent(k), percent(v))).collect::<Vec<_>>().join("&");
    let signature = hmac_sha1(&format!("{secret}&"), &format!("{method}&{}&{}", percent("/"), percent(&canonical)));
    format!("{canonical}&Signature={}", percent(&signature))
}

/// A ROA-style request's `Authorization` (`acs <key id>:<signature>`), over its method, its
/// `Accept`, `Content-MD5`, `Content-Type` and `Date`, its `x-acs-` headers (by lower-cased name),
/// and its path.
pub fn roa_authorization(method: &str, headers: &BTreeMap<String, String>, path: &str, creds: &Credentials) -> String {
    let get = |k: &str| headers.iter().find(|(h, _)| h.eq_ignore_ascii_case(k)).map_or("", |(_, v)| v.as_str());
    let acs: BTreeMap<String, &str> = headers.iter().filter(|(k, _)| k.to_ascii_lowercase().starts_with("x-acs-")).map(|(k, v)| (k.to_ascii_lowercase(), v.trim())).collect();
    let canonical: String = acs.iter().map(|(k, v)| format!("{k}:{v}\n")).collect();
    let text = format!("{method}\n{}\n{}\n{}\n{}\n{canonical}{path}", get("accept"), get("content-md5"), get("content-type"), get("date"));
    format!("acs {}:{}", creds.access_key_id, hmac_sha1(&creds.access_key_secret, &text))
}

/// Now, as RPC's `Timestamp` wants it: `2026-10-21T09:00:00Z`.
fn timestamp() -> String {
    let t = time::OffsetDateTime::now_utc();
    format!("{:04}-{:02}-{:02}T{:02}:{:02}:{:02}Z", t.year(), u8::from(t.month()), t.day(), t.hour(), t.minute(), t.second())
}

/// Now, as HTTP's `Date` says it: `Wed, 21 Oct 2026 09:00:00 GMT`.
pub fn http_date() -> String {
    let t = time::OffsetDateTime::now_utc();
    let day = ["Mon", "Tue", "Wed", "Thu", "Fri", "Sat", "Sun"].get(usize::from(t.weekday().number_days_from_monday())).copied().unwrap_or("Mon");
    let month = ["Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec"].get(usize::from(u8::from(t.month())).saturating_sub(1)).copied().unwrap_or("Jan");
    format!("{day}, {:02} {month} {:04} {:02}:{:02}:{:02} GMT", t.day(), t.year(), t.hour(), t.minute(), t.second())
}

/// The region a Container Registry host is in: an Enterprise Edition instance's
/// (`<name>-registry[-vpc].<region>.cr.aliyuncs.com`) or the Personal Edition's
/// (`registry[-vpc].<region>.aliyuncs.com`).
pub fn acr_region(host: &str) -> Option<String> {
    let host = host.strip_suffix(".aliyuncs.com")?;
    let region = match host.strip_suffix(".cr") {
        Some(enterprise) => enterprise.rsplit('.').next()?,
        None => host.strip_prefix("registry.").or_else(|| host.strip_prefix("registry-vpc."))?,
    };
    (!region.is_empty() && !region.contains('.')).then(|| region.to_owned())
}

/// An Alibaba answer's error, as it says it (`Code`, `Message`).
async fn refused(what: &str, r: crate::http::client::Response) -> anyhow::Error {
    let status = r.status();
    let v: Value = r.json(LIMIT).await.unwrap_or(Value::Null);
    let said = |k: &str| v.get(k).and_then(Value::as_str).unwrap_or_default().to_owned();
    anyhow!("{what} refused it ({status}): {} {}", said("Code"), said("Message"))
}

/// STS's `AssumeRoleWithOIDC`: the ID token for `role_arn`'s temporary credentials, trusted
/// through `provider_arn`.
pub async fn assume_role_with_oidc(http: &Client, region: Option<&str>, provider_arn: &str, role_arn: &str, session: &str, token: &str, seconds: u32) -> Result<Credentials> {
    let base = super::endpoint("alibaba:sts", || match region {
        Some(r) => format!("https://sts.{r}.aliyuncs.com"),
        None => "https://sts.aliyuncs.com".into(),
    });
    let mut form: BTreeMap<String, String> = BTreeMap::new();
    for (k, v) in
        [("Action", "AssumeRoleWithOIDC"), ("Format", "JSON"), ("Version", "2015-04-01"), ("OIDCProviderArn", provider_arn), ("RoleArn", role_arn), ("OIDCToken", token), ("RoleSessionName", session)]
    {
        form.insert(k.to_owned(), v.to_owned());
    }
    form.insert("Timestamp".into(), timestamp());
    form.insert("SignatureNonce".into(), crate::util::new_id());
    form.insert("DurationSeconds".into(), seconds.to_string());
    let body = form.iter().map(|(k, v)| format!("{}={}", percent(k), percent(v))).collect::<Vec<_>>().join("&");
    let r = http.post(&format!("{base}/")).header("Content-Type", "application/x-www-form-urlencoded").body(body).send().await.context("Alibaba's STS")?;
    if !r.is_success() {
        return Err(refused("Alibaba's STS", r).await);
    }
    let v: Value = r.json(LIMIT).await?;
    let c = v.get("Credentials").ok_or_else(|| anyhow!("Alibaba's STS answered without credentials"))?;
    let field = |k: &str| c.get(k).and_then(Value::as_str).map(str::to_owned).ok_or_else(|| anyhow!("Alibaba's credentials lack {k}"));
    Ok(Credentials { access_key_id: field("AccessKeyId")?, access_key_secret: field("AccessKeySecret")?, security_token: field("SecurityToken")? })
}

/// Container Registry's temporary username and password in `region`: an Enterprise Edition
/// instance's (`instance`), else the Personal Edition's.
pub async fn acr_login(http: &Client, region: &str, instance: Option<&str>, creds: &Credentials) -> Result<(String, String)> {
    let base = super::endpoint("alibaba:cr", || format!("https://cr.{region}.aliyuncs.com"));
    match instance {
        Some(id) => {
            let mut q: BTreeMap<String, String> = BTreeMap::new();
            for (k, v) in [
                ("Action", "GetAuthorizationToken"),
                ("Format", "JSON"),
                ("Version", "2018-12-01"),
                ("InstanceId", id),
                ("RegionId", region),
                ("AccessKeyId", creds.access_key_id.as_str()),
                ("SecurityToken", creds.security_token.as_str()),
                ("SignatureMethod", "HMAC-SHA1"),
                ("SignatureVersion", "1.0"),
            ] {
                q.insert(k.to_owned(), v.to_owned());
            }
            q.insert("Timestamp".into(), timestamp());
            q.insert("SignatureNonce".into(), crate::util::new_id());
            let r = http.get(&format!("{base}/?{}", rpc_query("GET", &q, &creds.access_key_secret))).send().await.context("Alibaba's Container Registry")?;
            if !r.is_success() {
                return Err(refused("Alibaba's Container Registry", r).await);
            }
            let v: Value = r.json(LIMIT).await?;
            let password = v.get("AuthorizationToken").and_then(Value::as_str).ok_or_else(|| anyhow!("Container Registry's answer has no AuthorizationToken"))?;
            let user = v.get("TempUsername").and_then(Value::as_str).unwrap_or("cr_temp_user");
            Ok((user.to_owned(), password.to_owned()))
        }
        None => {
            let mut h: BTreeMap<String, String> = BTreeMap::new();
            for (k, v) in [
                ("Accept", "application/json"),
                ("x-acs-signature-method", "HMAC-SHA1"),
                ("x-acs-signature-version", "1.0"),
                ("x-acs-version", "2016-06-07"),
                ("x-acs-region-id", region),
                ("x-acs-security-token", creds.security_token.as_str()),
            ] {
                h.insert(k.to_owned(), v.to_owned());
            }
            h.insert("Date".into(), http_date());
            h.insert("x-acs-signature-nonce".into(), crate::util::new_id());
            let auth = roa_authorization("GET", &h, "/tokens", creds);
            let mut req = http.get(&format!("{base}/tokens")).header("Authorization", &auth);
            for (k, v) in &h {
                req = req.header(k, v);
            }
            let r = req.send().await.context("Alibaba's Container Registry")?;
            if !r.is_success() {
                return Err(refused("Alibaba's Container Registry", r).await);
            }
            let v: Value = r.json(LIMIT).await?;
            let data = v.get("data").unwrap_or(&v);
            let password = data.get("authorizationToken").and_then(Value::as_str).ok_or_else(|| anyhow!("Container Registry's answer has no authorizationToken"))?;
            let user = data.get("tempUserName").and_then(Value::as_str).unwrap_or("cr_temp_user");
            Ok((user.to_owned(), password.to_owned()))
        }
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use std::collections::HashMap;
    use std::sync::Arc;

    use http::StatusCode;
    use serde_json::json;

    use crate::http::server::{self as srv, Request, Response};

    /// A test's own server for `h`: its base URL.
    pub(crate) fn serve<H: srv::Handler>(h: Arc<H>) -> anyhow::Result<String> {
        let tcp = hyper_rt::tcp::TcpListener::bind(std::net::SocketAddr::from(([127, 0, 0, 1], 0)), 16).map_err(|e| anyhow::anyhow!("{e}"))?;
        let addr = tcp.local_addr().map_err(|e| anyhow::anyhow!("{e}"))?;
        crate::rt::spawn(async move {
            while let Ok(s) = tcp.accept().await {
                let (h, r) = (Arc::clone(&h), s.peer_addr().ok());
                crate::rt::spawn(srv::serve_connection(crate::http::io::RtIo::new(s), h, r));
            }
        });
        Ok(format!("http://{addr}"))
    }

    /// STS and Container Registry as Alibaba answers them: the role's credentials for its ID
    /// token, and a password for a request its temporary key signed.
    struct Alibaba;

    impl srv::Handler for Alibaba {
        async fn handle(self: Arc<Self>, req: Request) -> Response {
            let (method, path) = (req.method().clone(), req.uri().path().to_owned());
            let q = srv::query(&req);
            let headers: BTreeMap<String, String> = req.headers().iter().map(|(k, v)| (k.as_str().to_owned(), v.to_str().unwrap_or_default().to_owned())).collect();
            let body = srv::body_bytes(req.into_body(), 1 << 20).await.unwrap_or_default();
            let creds = Credentials { access_key_id: "STS.id".into(), access_key_secret: "sk".into(), security_token: "st".into() };
            let no = |m: &str| srv::json(StatusCode::FORBIDDEN, &json!({ "Code": "Forbidden", "Message": m }));
            match (method.as_str(), path.as_str()) {
                ("POST", "/") => {
                    let form: HashMap<String, String> = String::from_utf8_lossy(&body).split('&').filter_map(|kv| kv.split_once('=')).map(|(k, v)| (k.to_owned(), srv::decode(v))).collect();
                    let get = |k: &str| form.get(k).map(String::as_str).unwrap_or_default();
                    if (get("Action"), get("OIDCToken"), get("RoleArn"), get("OIDCProviderArn")) != ("AssumeRoleWithOIDC", "jwt-1", "acs:ram::1:role/ci", "acs:ram::1:oidc-provider/kiln") {
                        return no(&format!("{form:?}"));
                    }
                    srv::json(StatusCode::OK, &json!({ "Credentials": { "AccessKeyId": "STS.id", "AccessKeySecret": "sk", "SecurityToken": "st", "Expiration": "2026-10-21T10:00:00Z" } }))
                }
                ("GET", "/") => {
                    let mut asked: BTreeMap<String, String> = q.clone().into_iter().collect();
                    let Some(signature) = asked.remove("Signature") else { return no("unsigned") };
                    let want = rpc_query("GET", &asked, "sk");
                    if !want.ends_with(&format!("Signature={}", percent(&signature)))
                        || asked.get("SecurityToken").map(String::as_str) != Some("st")
                        || asked.get("InstanceId").map(String::as_str) != Some("cri-1")
                    {
                        return no(&format!("{asked:?}"));
                    }
                    srv::json(StatusCode::OK, &json!({ "AuthorizationToken": "pw-enterprise", "TempUsername": "cr_temp_user", "IsSuccess": true }))
                }
                ("GET", "/tokens") => {
                    let given = headers.get("authorization").cloned().unwrap_or_default();
                    let mut signed = headers.clone();
                    signed.remove("authorization");
                    if given != roa_authorization("GET", &signed, "/tokens", &creds) || signed.get("x-acs-security-token").map(String::as_str) != Some("st") {
                        return no(&format!("{given} {signed:?}"));
                    }
                    srv::json(StatusCode::OK, &json!({ "data": { "authorizationToken": "pw-personal", "tempUserName": "cr_temp_user" } }))
                }
                _ => no(&format!("nothing at {method} {path}")),
            }
        }
    }

    /// W12-R03, D.2: the role's credentials for the leg's ID token, then Container Registry's
    /// password: an Enterprise Edition instance's, and the Personal Edition's.
    #[test]
    fn w12_r03_alibaba_container_registry_through_ram() -> anyhow::Result<()> {
        crate::rt::block_on(async {
            let base = serve(Arc::new(Alibaba))?;
            crate::ci::integrations::override_endpoint("alibaba:sts", &base);
            crate::ci::integrations::override_endpoint("alibaba:cr", &base);
            let http = crate::http::client::Client::public()?;
            let creds = assume_role_with_oidc(&http, Some("cn-hangzhou"), "acs:ram::1:oidc-provider/kiln", "acs:ram::1:role/ci", "kiln-acme", "jwt-1", 3600).await?;
            assert_eq!(format!("{creds:?}"), "Credentials { access_key_id: \"STS.id\", .. }", "its secret never printed");
            assert_eq!(acr_login(&http, "cn-hangzhou", Some("cri-1"), &creds).await?, ("cr_temp_user".to_owned(), "pw-enterprise".to_owned()));
            assert_eq!(acr_login(&http, "cn-hangzhou", None, &creds).await?, ("cr_temp_user".to_owned(), "pw-personal".to_owned()));
            let e = assume_role_with_oidc(&http, None, "acs:ram::1:oidc-provider/kiln", "acs:ram::1:role/ci", "kiln-acme", "jwt-other", 3600).await.err().map(|e| format!("{e:#}")).unwrap_or_default();
            assert!(e.contains("Forbidden"), "{e}");
            Ok(())
        })
    }

    /// Alibaba's own example of an RPC signature (the signature method's documentation).
    #[test]
    fn w12_r03_an_rpc_signature_as_alibaba_signs_it() {
        let q: BTreeMap<String, String> = [
            ("Timestamp", "2016-02-23T12:46:24Z"),
            ("Format", "XML"),
            ("AccessKeyId", "testid"),
            ("Action", "DescribeRegions"),
            ("SignatureMethod", "HMAC-SHA1"),
            ("SignatureNonce", "3ee8c1b8-83d3-44af-a94f-4e0ad82fd6cf"),
            ("Version", "2014-05-26"),
            ("SignatureVersion", "1.0"),
        ]
        .into_iter()
        .map(|(k, v)| (k.to_owned(), v.to_owned()))
        .collect();
        let query = rpc_query("GET", &q, "testsecret");
        assert!(query.ends_with("&Signature=OLeaidS1JvxuMvnyHOwuJ%2BuX5qY%3D"), "{query}");
        assert!(query.starts_with("AccessKeyId=testid&Action=DescribeRegions&Format=XML&SignatureMethod=HMAC-SHA1&"), "{query}");
        assert_eq!(percent("a b*c~d/é"), "a%20b%2Ac~d%2F%C3%A9");
    }

    #[test]
    fn w12_r03_container_registry_hosts() {
        assert_eq!(acr_region("acme-registry.cn-hangzhou.cr.aliyuncs.com").as_deref(), Some("cn-hangzhou"));
        assert_eq!(acr_region("acme-registry-vpc.ap-southeast-1.cr.aliyuncs.com").as_deref(), Some("ap-southeast-1"));
        assert_eq!(acr_region("registry.cn-beijing.aliyuncs.com").as_deref(), Some("cn-beijing"));
        assert_eq!(acr_region("registry-vpc.us-west-1.aliyuncs.com").as_deref(), Some("us-west-1"));
        assert_eq!(acr_region("ghcr.io"), None);
        let creds = Credentials { access_key_id: "id".into(), access_key_secret: "secret".into(), security_token: "st".into() };
        let h: BTreeMap<String, String> = [("Accept", "application/json"), ("Date", "Wed, 21 Oct 2026 09:00:00 GMT"), ("x-acs-version", "2016-06-07"), ("X-Acs-Signature-Nonce", "n")]
            .into_iter()
            .map(|(k, v)| (k.to_owned(), v.to_owned()))
            .collect();
        let text = "GET\napplication/json\n\n\nWed, 21 Oct 2026 09:00:00 GMT\nx-acs-signature-nonce:n\nx-acs-version:2016-06-07\n/tokens";
        assert_eq!(roa_authorization("GET", &h, "/tokens", &creds), format!("acs id:{}", hmac_sha1("secret", text)));
        assert!(http_date().ends_with(" GMT") && http_date().len() == 29, "{}", http_date());
    }
}
