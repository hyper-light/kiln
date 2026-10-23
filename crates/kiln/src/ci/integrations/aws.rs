//! AWS (docs/ci.md W11; docs/cd-targets.md §6): credentials from kiln's own identity, and the
//! requests AWS providers make. There is no SDK (it needs tokio): requests are signed here with
//! Signature Version 4, in the three protocols AWS's services speak, as the providers need them:
//!
//! - **JSON** (ECS): `POST /`, the operation in `X-Amz-Target`;
//! - **REST-JSON** (Lambda): the operation in the method and path;
//! - **Query** (STS, Auto Scaling, CloudFormation): a form, answered in XML.
//!
//! Credentials come from `AssumeRoleWithWebIdentity` with a kiln ID token minted for the workload
//! (a CD sync, say: `kiln_ci::oidc::Workload`), so no key is stored. They last 15 minutes, are
//! kept in memory until two minutes before they expire, and are never written down or printed.

use std::collections::HashMap;
use std::sync::OnceLock;

use anyhow::{Context as _, Result, bail};
use parking_lot::Mutex;
use serde_json::Value;

use super::Integration;
use crate::hub::Hub;
use crate::http::client::{Client, Method, Roots, Response};

/// The largest answer read from AWS.
const LIMIT: usize = 32 << 20;

/// How long minted credentials are asked for (STS's least).
const LIFETIME_SECS: u32 = 900;

/// AWS credentials: a key pair, and a session token when they are temporary.
#[derive(Clone, PartialEq, Eq)]
pub struct Credentials {
    pub access_key_id: String,
    pub secret_access_key: String,
    pub session_token: Option<String>,
    /// When they expire (Unix seconds), for temporary ones.
    pub expires_at: Option<i64>,
}

impl std::fmt::Debug for Credentials {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Credentials").field("access_key_id", &self.access_key_id).field("secret_access_key", &"<redacted>").field("expires_at", &self.expires_at).finish()
    }
}

/// An error AWS answered: its HTTP status, its code (`ResourceNotFoundException`,
/// `AccessDenied`) and its message.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AwsError {
    pub status: u16,
    pub code: String,
    pub message: String,
}

impl std::fmt::Display for AwsError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        if self.message.is_empty() { write!(f, "{} (HTTP {})", self.code, self.status) } else { write!(f, "{}: {} (HTTP {})", self.code, self.message, self.status) }
    }
}

impl std::error::Error for AwsError {}

impl AwsError {
    /// Whether `e` is AWS saying something doesn't exist.
    pub fn is_not_found(e: &anyhow::Error) -> bool {
        e.downcast_ref::<AwsError>().is_some_and(|a| a.status == 404 || a.code.ends_with("NotFoundException") || a.code.ends_with("NotFound") || a.code == "NoSuchEntity")
    }
}

// ------------------------------------------------------------------ signing

/// AWS's URI encoding: RFC 3986's unreserved characters as they are, everything else `%XX` in
/// upper case; `/` too, unless `keep_slash` (paths).
pub fn encode(s: &str, keep_slash: bool) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.' | b'~') || (keep_slash && b == b'/') {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

fn hmac(key: &[u8], msg: &[u8]) -> Vec<u8> {
    use hmac::{KeyInit as _, Mac as _};
    match hmac::Hmac::<sha2::Sha256>::new_from_slice(key) {
        Ok(mut m) => {
            m.update(msg);
            m.finalize().into_bytes().to_vec()
        }
        Err(_) => Vec::new(),
    }
}

/// The key a day's requests to `service` in `region` are signed with.
pub fn signing_key(secret: &str, date: &str, region: &str, service: &str) -> Vec<u8> {
    let k = hmac(format!("AWS4{secret}").as_bytes(), date.as_bytes());
    let k = hmac(&k, region.as_bytes());
    let k = hmac(&k, service.as_bytes());
    hmac(&k, b"aws4_request")
}

/// What Signature Version 4 signs of a request.
pub struct Signable<'a> {
    pub method: &'a str,
    /// The path as it is sent: each segment encoded once.
    pub path: &'a str,
    /// The query's pairs, not encoded.
    pub query: &'a [(String, String)],
    /// The headers to sign, `host` among them.
    pub headers: &'a [(String, String)],
    /// The body's SHA-256, in hex.
    pub payload_sha256: &'a str,
}

/// The canonical request, and the signed headers' names. Every service but S3 encodes the path
/// a second time (`double`).
pub fn canonical_request(r: &Signable<'_>, double: bool) -> (String, String) {
    let path = if r.path.is_empty() { "/".to_owned() } else if double { encode(r.path, true) } else { r.path.to_owned() };
    let mut query: Vec<(String, String)> = r.query.iter().map(|(k, v)| (encode(k, false), encode(v, false))).collect();
    query.sort();
    let query: Vec<String> = query.into_iter().map(|(k, v)| format!("{k}={v}")).collect();
    let mut headers: Vec<(String, String)> = r.headers.iter().map(|(k, v)| (k.to_ascii_lowercase(), v.split_whitespace().collect::<Vec<_>>().join(" "))).collect();
    headers.sort();
    let mut merged: Vec<(String, String)> = Vec::new();
    for (k, v) in headers {
        match merged.last_mut() {
            Some((lk, lv)) if *lk == k => {
                lv.push(',');
                lv.push_str(&v);
            }
            _ => merged.push((k, v)),
        }
    }
    let canonical_headers: String = merged.iter().map(|(k, v)| format!("{k}:{v}\n")).collect();
    let signed: Vec<&str> = merged.iter().map(|(k, _)| k.as_str()).collect();
    let signed = signed.join(";");
    (format!("{}\n{path}\n{}\n{canonical_headers}\n{signed}\n{}", r.method, query.join("&"), r.payload_sha256), signed)
}

/// The `Authorization` header for `r`, signed at `amz_date` (`20150830T123600Z`).
pub fn authorization(creds: &Credentials, region: &str, service: &str, amz_date: &str, r: &Signable<'_>, double: bool) -> String {
    let date = amz_date.get(..8).unwrap_or(amz_date);
    let scope = format!("{date}/{region}/{service}/aws4_request");
    let (canonical, signed) = canonical_request(r, double);
    let to_sign = format!("AWS4-HMAC-SHA256\n{amz_date}\n{scope}\n{}", crate::util::sha256_hex(canonical.as_bytes()));
    let signature = hex::encode(hmac(&signing_key(&creds.secret_access_key, date, region, service), to_sign.as_bytes()));
    format!("AWS4-HMAC-SHA256 Credential={}/{scope}, SignedHeaders={signed}, Signature={signature}", creds.access_key_id)
}

/// AWS's timestamp for now: `20150830T123600Z`.
pub fn amz_date_now() -> String {
    let t = time::OffsetDateTime::now_utc();
    format!("{:04}{:02}{:02}T{:02}{:02}{:02}Z", t.year(), u8::from(t.month()), t.day(), t.hour(), t.minute(), t.second())
}

// ------------------------------------------------------------------ requests

fn client() -> Result<&'static Client> {
    static C: OnceLock<Client> = OnceLock::new();
    if let Some(c) = C.get() {
        return Ok(c);
    }
    let c = Client::new(Roots::WebPki, None)?;
    Ok(C.get_or_init(|| c))
}

/// A service's endpoint in a region: `https://ecs.us-east-1.amazonaws.com` (or the fake a test
/// put in its place, as `aws:<service>`).
pub fn endpoint(service: &str, region: &str) -> String {
    super::endpoint(&format!("aws:{service}"), || {
        let domain = if region.starts_with("cn-") { "amazonaws.com.cn" } else { "amazonaws.com" };
        format!("https://{service}.{region}.{domain}")
    })
}

/// Sends `service`'s requests to `base` instead (a fake, in tests).
pub fn override_endpoint(service: &str, base: &str) {
    super::override_endpoint(&format!("aws:{service}"), base);
}

/// A client for one region, with credentials.
#[derive(Clone, Debug)]
pub struct Aws {
    pub creds: Credentials,
    pub region: String,
}

/// One request to AWS.
pub struct Call<'a> {
    /// The service's signing name (`ecs`, `lambda`).
    pub service: &'a str,
    /// Where it goes (`https://ecs.us-east-1.amazonaws.com`).
    pub base: &'a str,
    pub method: Method,
    /// The path, each segment encoded once.
    pub path: &'a str,
    pub query: &'a [(String, String)],
    pub headers: Vec<(String, String)>,
    pub body: Body,
}

/// What a request carries.
pub enum Body {
    Bytes(Vec<u8>),
    /// Streamed, its length and SHA-256 (hex) known ahead: S3 signs the hash and wants the length.
    Stream { rx: async_channel::Receiver<bytes::Bytes>, len: u64, sha256: String },
}

impl From<Vec<u8>> for Body {
    fn from(b: Vec<u8>) -> Self {
        Body::Bytes(b)
    }
}

impl Aws {
    /// Signs and sends `call`.
    pub async fn send(&self, call: Call<'_>) -> Result<Response> {
        self.send_with(client()?, call).await
    }

    /// Signs and sends `call` with `http` (one that trusts a private store's CA, say).
    pub async fn send_with(&self, http: &Client, call: Call<'_>) -> Result<Response> {
        let host = call.base.split_once("://").map_or(call.base, |(_, h)| h).trim_end_matches('/').to_owned();
        let amz_date = amz_date_now();
        let payload = match &call.body {
            Body::Bytes(b) => crate::util::sha256_hex(b),
            Body::Stream { sha256, .. } => sha256.clone(),
        };
        let mut headers = call.headers;
        if let Body::Stream { len, .. } = &call.body {
            headers.push(("content-length".into(), len.to_string()));
        }
        headers.push(("host".into(), host));
        headers.push(("x-amz-date".into(), amz_date.clone()));
        if let Some(t) = &self.creds.session_token {
            headers.push(("x-amz-security-token".into(), t.clone()));
        }
        if call.service == "s3" {
            headers.push(("x-amz-content-sha256".into(), payload.clone()));
        }
        let method = method_name(call.method);
        let auth = authorization(&self.creds, &self.region, call.service, &amz_date, &Signable { method, path: call.path, query: call.query, headers: &headers, payload_sha256: &payload }, call.service != "s3");
        let query: Vec<String> = call.query.iter().map(|(k, v)| format!("{}={}", encode(k, false), encode(v, false))).collect();
        let url = if query.is_empty() { format!("{}{}", call.base.trim_end_matches('/'), call.path) } else { format!("{}{}?{}", call.base.trim_end_matches('/'), call.path, query.join("&")) };
        // An upload's answer comes once it's all sent.
        let wait = if matches!(call.body, Body::Stream { .. }) { 1800 } else { 60 };
        let mut req = http.request(call.method, &url).timeout(std::time::Duration::from_secs(wait)).header("authorization", &auth);
        for (k, v) in headers.iter().filter(|(k, _)| k != "host") {
            req = req.header(k, v);
        }
        let req = match call.body {
            Body::Bytes(b) => req.body(b),
            Body::Stream { rx, .. } => req.body_stream(rx),
        };
        req.send().await.with_context(|| format!("AWS {} {}", call.service, call.path))
    }

    /// An operation of a JSON-protocol service (ECS): `target` is
    /// `AmazonEC2ContainerServiceV20141113.DescribeServices`, say.
    pub async fn json(&self, service: &str, target: &str, body: &Value) -> Result<Value> {
        let base = endpoint(service, &self.region);
        let headers = vec![("content-type".into(), "application/x-amz-json-1.1".into()), ("x-amz-target".into(), target.to_owned())];
        let resp = self.send(Call { service, base: &base, method: Method::Post, path: "/", query: &[], headers, body: serde_json::to_vec(body)?.into() }).await?;
        json_answer(resp).await
    }

    /// An operation of a REST-JSON service (Lambda).
    pub async fn rest(&self, service: &str, method: Method, path: &str, query: &[(String, String)], body: Option<&Value>) -> Result<Value> {
        let base = endpoint(service, &self.region);
        let (headers, body) = match body {
            Some(b) => (vec![("content-type".to_owned(), "application/json".to_owned())], serde_json::to_vec(b)?),
            None => (Vec::new(), Vec::new()),
        };
        let resp = self.send(Call { service, base: &base, method, path, query, headers, body: body.into() }).await?;
        json_answer(resp).await
    }

    /// An operation of a Query-protocol service (Auto Scaling, CloudFormation): its XML answer.
    pub async fn query(&self, service: &str, action: &str, version: &str, params: &[(String, String)]) -> Result<String> {
        let base = endpoint(service, &self.region);
        let mut form = vec![("Action".to_owned(), action.to_owned()), ("Version".to_owned(), version.to_owned())];
        form.extend(params.iter().cloned());
        let body = form_body(&form);
        let headers = vec![("content-type".to_owned(), "application/x-www-form-urlencoded; charset=utf-8".to_owned())];
        let resp = self.send(Call { service, base: &base, method: Method::Post, path: "/", query: &[], headers, body: body.into_bytes().into() }).await?;
        xml_answer(resp).await
    }
}

impl Aws {
    /// The account these credentials act in (STS GetCallerIdentity).
    pub async fn account(&self) -> Result<String> {
        let xml = self.query("sts", "GetCallerIdentity", "2011-06-15", &[]).await?;
        xml_text(&xml, "Account").ok_or_else(|| anyhow::anyhow!("STS didn't say which account this is"))
    }
}

fn method_name(m: Method) -> &'static str {
    match m {
        Method::Get => "GET",
        Method::Post => "POST",
        Method::Put => "PUT",
        Method::Patch => "PATCH",
        Method::Delete => "DELETE",
        Method::Head => "HEAD",
    }
}

fn form_body(pairs: &[(String, String)]) -> String {
    pairs.iter().map(|(k, v)| format!("{}={}", encode(k, false), encode(v, false))).collect::<Vec<_>>().join("&")
}

/// A JSON answer, or the error it carries (`__type` or `x-amzn-ErrorType`, and `message`).
async fn json_answer(resp: Response) -> Result<Value> {
    let status = resp.status();
    let kind = resp.header("x-amzn-errortype").map(str::to_owned);
    let bytes = resp.bytes(LIMIT).await?;
    if (200..300).contains(&status) {
        if bytes.is_empty() {
            return Ok(Value::Null);
        }
        return serde_json::from_slice(&bytes).context("AWS answered something that isn't JSON");
    }
    let v: Value = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
    let code = kind.or_else(|| v.get("__type").and_then(Value::as_str).map(str::to_owned)).or_else(|| v.get("code").and_then(Value::as_str).map(str::to_owned)).unwrap_or_else(|| "Error".into());
    // `__type` may carry a namespace (`com.amazonaws.ecs#ClientException`), the header a suffix.
    let code = code.rsplit('#').next().unwrap_or(&code).split(':').next().unwrap_or(&code).to_owned();
    let message = ["message", "Message", "errorMessage"].iter().find_map(|k| v.get(*k).and_then(Value::as_str)).unwrap_or_default().to_owned();
    let message = if message.is_empty() && v.is_null() { String::from_utf8_lossy(&bytes).chars().take(300).collect() } else { message };
    Err(AwsError { status, code, message }.into())
}

/// An XML answer, or the error it carries (`<Error><Code>…</Code><Message>…</Message>`).
async fn xml_answer(resp: Response) -> Result<String> {
    let status = resp.status();
    let text = String::from_utf8_lossy(&resp.bytes(LIMIT).await?).into_owned();
    if (200..300).contains(&status) {
        return Ok(text);
    }
    let code = xml_text(&text, "Code").unwrap_or_else(|| "Error".into());
    let message = xml_text(&text, "Message").unwrap_or_else(|| text.chars().take(300).collect());
    Err(AwsError { status, code, message }.into())
}

// ------------------------------------------------------------------ XML

/// The text of the first `<tag>` element in `xml`, unescaped.
pub fn xml_text(xml: &str, tag: &str) -> Option<String> {
    xml_all(xml, tag).first().map(|s| unescape(s))
}

/// The inner XML of every `<tag>` element in `xml`, in order; elements of the same name inside
/// one are part of its inner XML, not listed apart.
pub fn xml_all<'a>(xml: &'a str, tag: &str) -> Vec<&'a str> {
    let (open, close) = (format!("<{tag}"), format!("</{tag}>"));
    let mut out = Vec::new();
    let mut rest = xml;
    while let Some(i) = rest.find(&open) {
        let Some(after) = rest.get(i + open.len()..) else { break };
        // `<Code>` and `<Code attr="…">`, not `<CodeSize>`.
        if !matches!(after.chars().next(), Some('>' | ' ' | '/')) {
            rest = after;
            continue;
        }
        let Some((head, body)) = after.split_once('>') else { break };
        if head.ends_with('/') {
            out.push("");
            rest = body;
            continue;
        }
        let Some(end) = close_of(body, &open, &close) else { break };
        out.push(body.get(..end).unwrap_or_default());
        rest = body.get(end + close.len()..).unwrap_or_default();
    }
    out
}

/// Where the `close` that ends an element whose content starts `body` is, skipping elements of
/// the same name inside it.
fn close_of(body: &str, open: &str, close: &str) -> Option<usize> {
    let mut depth = 1usize;
    let mut pos = 0usize;
    while let Some(rest) = body.get(pos..) {
        let c = rest.find(close)?;
        match rest.find(open).filter(|&o| o < c) {
            Some(o) => {
                let after = rest.get(o + open.len()..)?;
                let gt = after.find('>')?;
                let real = matches!(after.chars().next(), Some('>' | ' ' | '/'));
                if real && !after.get(..gt)?.ends_with('/') {
                    depth += 1;
                }
                pos += o + open.len() + gt + 1;
            }
            None => {
                depth -= 1;
                if depth == 0 {
                    return Some(pos + c);
                }
                pos += c + close.len();
            }
        }
    }
    None
}

/// XML's five entities and numeric references, decoded.
pub fn unescape(s: &str) -> String {
    if !s.contains('&') {
        return s.to_owned();
    }
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    while let Some((before, after)) = rest.split_once('&') {
        out.push_str(before);
        let Some((entity, tail)) = after.split_once(';') else {
            out.push('&');
            out.push_str(after);
            return out;
        };
        let decoded = match entity {
            "amp" => Some('&'),
            "lt" => Some('<'),
            "gt" => Some('>'),
            "quot" => Some('"'),
            "apos" => Some('\''),
            e => e.strip_prefix("#x").and_then(|h| u32::from_str_radix(h, 16).ok()).or_else(|| e.strip_prefix('#').and_then(|d| d.parse().ok())).and_then(char::from_u32),
        };
        match decoded {
            Some(c) => {
                out.push(c);
                rest = tail;
            }
            None => {
                out.push('&');
                rest = after;
            }
        }
    }
    out.push_str(rest);
    out
}

/// A Query-protocol answer as JSON, in the shape AWS documents it: each element its name's
/// field, a list's members (`<member>`, EC2's `<item>`) an array, an element of text its text
/// (unescaped), an empty one `""`. Attributes, the declaration and comments are let go; an element
/// named twice among its siblings is an array of them.
pub fn xml_json(xml: &str) -> Value {
    let mut rest = xml;
    let mut root = Node::default();
    parse_children(&mut rest, &mut root);
    node_value(&root)
}

#[derive(Default)]
struct Node {
    name: String,
    text: String,
    children: Vec<Node>,
}

/// Reads elements into `parent` until its end tag (or the input's end).
fn parse_children(rest: &mut &str, parent: &mut Node) {
    loop {
        let Some(lt) = rest.find('<') else {
            parent.text.push_str(rest);
            *rest = "";
            return;
        };
        parent.text.push_str(rest.get(..lt).unwrap_or_default());
        let tag = rest.get(lt..).unwrap_or_default();
        // A declaration, a comment, CDATA: what's in CDATA is text.
        if let Some(after) = tag.strip_prefix("<![CDATA[") {
            let end = after.find("]]>").unwrap_or(after.len());
            parent.text.push_str(&escape_text(after.get(..end).unwrap_or_default()));
            *rest = after.get(end + 3..).unwrap_or_default();
            continue;
        }
        if tag.starts_with("<?") || tag.starts_with("<!") {
            let close = if tag.starts_with("<!--") { "-->" } else { ">" };
            *rest = tag.find(close).and_then(|e| tag.get(e + close.len()..)).unwrap_or_default();
            continue;
        }
        let Some(gt) = tag.find('>') else {
            *rest = "";
            return;
        };
        let inner = tag.get(1..gt).unwrap_or_default();
        *rest = tag.get(gt + 1..).unwrap_or_default();
        if inner.starts_with('/') {
            return;
        }
        let selfclosing = inner.ends_with('/');
        let name = inner.trim_end_matches('/').split_whitespace().next().unwrap_or_default().to_owned();
        let mut node = Node { name, ..Node::default() };
        if !selfclosing {
            parse_children(rest, &mut node);
        }
        parent.children.push(node);
    }
}

/// Text back as XML has it, for [`unescape`] to read once.
fn escape_text(s: &str) -> String {
    s.replace('&', "&amp;").replace('<', "&lt;")
}

fn node_value(n: &Node) -> Value {
    if n.children.is_empty() {
        return Value::String(unescape(n.text.trim()));
    }
    if n.children.iter().all(|c| c.name == "member" || c.name == "item") {
        return Value::Array(n.children.iter().map(node_value).collect());
    }
    let mut seen: HashMap<&str, usize> = HashMap::new();
    for c in &n.children {
        *seen.entry(c.name.as_str()).or_default() += 1;
    }
    let mut m = serde_json::Map::new();
    for c in &n.children {
        let v = node_value(c);
        if seen.get(c.name.as_str()).copied().unwrap_or(0) > 1 {
            if let Value::Array(a) = m.entry(c.name.clone()).or_insert_with(|| Value::Array(Vec::new())) {
                a.push(v);
            }
        } else {
            m.insert(c.name.clone(), v);
        }
    }
    Value::Object(m)
}

/// The params of a Query-protocol request for a list: `Name.member.1=a&Name.member.2=b`.
pub fn list_params(name: &str, items: &[String]) -> Vec<(String, String)> {
    items.iter().enumerate().map(|(i, v)| (format!("{name}.member.{}", i + 1), v.clone())).collect()
}

/// The params for a list of key-value pairs: `Tags.member.1.Key=team&Tags.member.1.Value=shop`.
pub fn pairs_params(name: &str, key: &str, value: &str, pairs: &[(String, String)]) -> Vec<(String, String)> {
    pairs.iter().enumerate().flat_map(|(i, (k, v))| [(format!("{name}.member.{}.{key}", i + 1), k.clone()), (format!("{name}.member.{}.{value}", i + 1), v.clone())]).collect()
}

// ------------------------------------------------------------------ STS

/// Exchanges a web identity token for a role's credentials. Not signed: the token is the
/// authentication. `base` is STS's endpoint (`https://sts.us-east-1.amazonaws.com`).
pub async fn assume_role_with_web_identity(base: &str, role_arn: &str, session: &str, token: &str, seconds: u32) -> Result<Credentials> {
    let form = [
        ("Action".to_owned(), "AssumeRoleWithWebIdentity".to_owned()),
        ("Version".to_owned(), "2011-06-15".to_owned()),
        ("RoleArn".to_owned(), role_arn.to_owned()),
        ("RoleSessionName".to_owned(), session.to_owned()),
        ("WebIdentityToken".to_owned(), token.to_owned()),
        ("DurationSeconds".to_owned(), seconds.to_string()),
    ];
    let resp = client()?
        .post(&format!("{}/", base.trim_end_matches('/')))
        .timeout(std::time::Duration::from_secs(30))
        .header("content-type", "application/x-www-form-urlencoded; charset=utf-8")
        .body(form_body(&form))
        .send()
        .await
        .context("AWS STS")?;
    let xml = xml_answer(resp).await?;
    let creds = xml_all(&xml, "Credentials").first().map(|c| c.to_string()).ok_or_else(|| anyhow::anyhow!("STS answered without credentials"))?;
    let field = |t: &str| xml_text(&creds, t).ok_or_else(|| anyhow::anyhow!("STS's credentials lack {t}"));
    let expires_at = xml_text(&creds, "Expiration").and_then(|e| time::OffsetDateTime::parse(&e, &time::format_description::well_known::Rfc3339).ok()).map(|t| t.unix_timestamp());
    Ok(Credentials { access_key_id: field("AccessKeyId")?, secret_access_key: field("SecretAccessKey")?, session_token: Some(field("SessionToken")?), expires_at })
}

/// A role session's name, as STS takes it: `[\w+=,.@-]`, 2 to 64 characters.
pub fn session_name(s: &str) -> String {
    let mut n: String = s.chars().map(|c| if c.is_ascii_alphanumeric() || "_+=,.@-".contains(c) { c } else { '-' }).take(64).collect();
    while n.len() < 2 {
        n.push('-');
    }
    n
}

/// STS's endpoint for `region` (the global one without).
pub fn sts_endpoint(region: Option<&str>) -> String {
    match region.filter(|r| !r.is_empty()) {
        Some(r) => endpoint("sts", r),
        None => super::endpoint("aws:sts", || "https://sts.amazonaws.com".into()),
    }
}

// ------------------------------------------------------------------ minting

/// Credentials minted lately, by integration and workload, until shortly before they expire.
fn minted() -> &'static Mutex<HashMap<String, Credentials>> {
    static M: OnceLock<Mutex<HashMap<String, Credentials>>> = OnceLock::new();
    M.get_or_init(|| Mutex::new(HashMap::new()))
}

/// An `aws` integration's credentials for `w`: kiln's ID token for the workload, exchanged for
/// the role's (W11-R03). Kept until two minutes before they expire.
pub async fn credentials(hub: &Hub, i: &Integration, w: &kiln_ci::oidc::Workload<'_>) -> Result<Credentials> {
    let cfg = |k: &str| i.config.get(k).and_then(Value::as_str).map(str::trim).filter(|s| !s.is_empty()).map(str::to_owned);
    let Some(role) = cfg("role_arn") else { bail!("{} has no role_arn", i.name) };
    let audience = cfg("audience").unwrap_or_else(|| "sts.amazonaws.com".into());
    let key = format!("{}|{}|{}|{}|{}|{}", i.id, w.repository, w.environment.unwrap_or(""), w.app.unwrap_or(""), w.trigger.unwrap_or(""), role);
    let now = crate::util::now_unix();
    if let Some(c) = minted().lock().get(&key).filter(|c| c.expires_at.is_some_and(|e| e - 120 > now)) {
        return Ok(c.clone());
    }
    let issuer = crate::ci::oidc::issuer(hub);
    let claims = kiln_ci::oidc::workload_claims(w, &issuer, &audience, &crate::util::random_token(), now, 300, None);
    let token = crate::ci::oidc::mint(hub, &claims).await?;
    let session = session_name(&format!("kiln-{}", w.app.unwrap_or(w.source)));
    let c = assume_role_with_web_identity(&sts_endpoint(cfg("region").as_deref()), &role, &session, &token, LIFETIME_SECS).await.with_context(|| format!("assuming {role} as {}", claims.get("sub").and_then(Value::as_str).unwrap_or("kiln")))?;
    minted().lock().insert(key, c.clone());
    crate::audit::exchange(hub, i, w, &role).await;
    Ok(c)
}

/// What to check when AWS refuses kiln's token, by its error code.
pub fn advice(e: &anyhow::Error) -> Option<&'static str> {
    let a = e.downcast_ref::<AwsError>()?;
    Some(match a.code.as_str() {
        "InvalidIdentityToken" => "AWS can't check kiln's token: add kiln's issuer as an IAM OpenID Connect provider, and make sure AWS can fetch its keys (a public URL, or `kiln oidc publish`)",
        "AccessDenied" => "the role's trust policy doesn't accept this token: check its audience and its sub condition",
        "IDPRejectedClaim" => "the role's identity provider rejected the token's claims",
        "ExpiredTokenException" => "the token expired before AWS read it: check this machine's clock",
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn query_answers_read_as_json() {
        let xml = r#"<?xml version="1.0"?><DescribeStacksResponse xmlns="http://cloudformation.amazonaws.com/doc/2010-05-15/"><DescribeStacksResult><Stacks><member><StackName>net</StackName><Parameters><member><ParameterKey>Env</ParameterKey><ParameterValue>a &amp; b</ParameterValue></member></Parameters><Tags/><Description><![CDATA[<x> & y]]></Description></member></Stacks></DescribeStacksResult><!-- a note --><ResponseMetadata><RequestId>r1</RequestId></ResponseMetadata></DescribeStacksResponse>"#;
        let v = xml_json(xml);
        let stack = v.pointer("/DescribeStacksResponse/DescribeStacksResult/Stacks/0").cloned().unwrap_or_default();
        assert_eq!(stack.get("StackName"), Some(&Value::String("net".into())));
        assert_eq!(stack.pointer("/Parameters/0/ParameterValue"), Some(&Value::String("a & b".into())));
        assert_eq!(stack.get("Tags"), Some(&Value::String(String::new())), "an empty list reads as empty");
        assert_eq!(stack.get("Description"), Some(&Value::String("<x> & y".into())));
        assert_eq!(v.pointer("/DescribeStacksResponse/ResponseMetadata/RequestId"), Some(&Value::String("r1".into())));
        let ec2 = xml_json("<R><launchTemplateVersionSet><item><versionNumber>3</versionNumber></item><item><versionNumber>2</versionNumber></item></launchTemplateVersionSet></R>");
        assert_eq!(ec2.pointer("/R/launchTemplateVersionSet/1/versionNumber"), Some(&Value::String("2".into())));
        let repeated = xml_json("<R><a>1</a><a>2</a><a>3</a><b>x</b></R>");
        assert_eq!(repeated.pointer("/R/a"), Some(&serde_json::json!(["1", "2", "3"])));
        assert_eq!(list_params("Capabilities", &["CAPABILITY_IAM".into()]), [("Capabilities.member.1".to_owned(), "CAPABILITY_IAM".to_owned())]);
        assert_eq!(pairs_params("Tags", "Key", "Value", &[("team".into(), "shop".into())]).len(), 2);
    }

    /// AWS's own example of deriving a signing key (date 20120215, us-east-1, iam).
    #[test]
    fn signing_keys_derive_as_aws_documents() {
        let k = signing_key("wJalrXUtnFEMI/K7MDENG+bPxRfiCYEXAMPLEKEY", "20120215", "us-east-1", "iam");
        assert_eq!(hex::encode(k), "f4780e2d9f65fa895f9c67b32ce1baf0b0d8a43505a000a1a9e090d414db404d");
    }

    /// AWS's worked example: IAM ListUsers, signed at 20150830T123600Z.
    #[test]
    fn requests_sign_as_aws_documents() {
        let creds = Credentials { access_key_id: "AKIDEXAMPLE".into(), secret_access_key: "wJalrXUtnFEMI/K7MDENG+bPxRfiCYEXAMPLEKEY".into(), session_token: None, expires_at: None };
        let query = [("Action".to_owned(), "ListUsers".to_owned()), ("Version".to_owned(), "2010-05-08".to_owned())];
        let headers = [
            ("Content-Type".to_owned(), "application/x-www-form-urlencoded; charset=utf-8".to_owned()),
            ("Host".to_owned(), "iam.amazonaws.com".to_owned()),
            ("X-Amz-Date".to_owned(), "20150830T123600Z".to_owned()),
        ];
        let empty = crate::util::sha256_hex(b"");
        let r = Signable { method: "GET", path: "/", query: &query, headers: &headers, payload_sha256: &empty };
        let (canonical, signed) = canonical_request(&r, true);
        assert_eq!(signed, "content-type;host;x-amz-date");
        assert_eq!(crate::util::sha256_hex(canonical.as_bytes()), "f536975d06c0309214f805bb90ccff089219ecd68b2577efef23edd43b7e1a59");
        let auth = authorization(&creds, "us-east-1", "iam", "20150830T123600Z", &r, true);
        assert_eq!(auth, "AWS4-HMAC-SHA256 Credential=AKIDEXAMPLE/20150830/us-east-1/iam/aws4_request, SignedHeaders=content-type;host;x-amz-date, Signature=5d672d79c15b13162d9279b0855cfba6789a8edb4c82c400e06b5924a6f2b5d7");
    }

    #[test]
    fn paths_and_values_encode_as_aws_wants() {
        assert_eq!(encode("a b/c~d", true), "a%20b/c~d");
        assert_eq!(encode("a b/c", false), "a%20b%2Fc");
        assert_eq!(encode("arn:aws:lambda", false), "arn%3Aaws%3Alambda");
        // A path segment encoded once is encoded again in the canonical request.
        let r = Signable { method: "GET", path: "/2015-03-31/functions/arn%3Aaws%3Alambda/configuration", query: &[], headers: &[], payload_sha256: "x" };
        assert!(canonical_request(&r, true).0.contains("/functions/arn%253Aaws%253Alambda/"));
        assert!(canonical_request(&r, false).0.contains("/functions/arn%3Aaws%3Alambda/"));
        assert_eq!(session_name("kiln-api/web"), "kiln-api-web");
        assert_eq!(session_name("a"), "a-");
    }

    #[test]
    fn xml_reads_what_aws_answers() {
        let xml = r#"<ErrorResponse xmlns="https://sts.amazonaws.com/doc/2011-06-15/"><Error><Type>Sender</Type><Code>AccessDenied</Code><Message>Not authorized &amp; &lt;denied&gt; &#39;x&#39;</Message></Error><RequestId>1</RequestId></ErrorResponse>"#;
        assert_eq!(xml_text(xml, "Code").as_deref(), Some("AccessDenied"));
        assert_eq!(xml_text(xml, "Message").as_deref(), Some("Not authorized & <denied> 'x'"));
        assert_eq!(xml_text(xml, "Request"), None);
        let list = "<Items><member><Name>a</Name></member><member><Name>b</Name><Empty/></member></Items>";
        let names: Vec<String> = xml_all(list, "member").iter().filter_map(|m| xml_text(m, "Name")).collect();
        assert_eq!(names, vec!["a", "b"]);
        assert_eq!(xml_all(list, "Empty"), vec![""]);
        // Lists inside list items stay inside their item.
        let nested = "<Groups><member><Name>g</Name><Tags><member><Key>a</Key></member></Tags></member><member><Name>h</Name></member></Groups>";
        let names: Vec<String> = xml_all(nested, "member").iter().filter_map(|m| xml_text(m, "Name")).collect();
        assert_eq!(names, vec!["g", "h"]);
    }

    /// STS and a JSON service, faked: credentials come from the web identity exchange, and the
    /// request after it is signed with them; errors carry AWS's code.
    #[test]
    fn credentials_come_from_sts_and_sign_requests() -> anyhow::Result<()> {
        use std::sync::Arc;

        use crate::http::server::{self as srv, Request, Response};

        struct Fake;
        impl srv::Handler for Fake {
            async fn handle(self: Arc<Self>, req: Request) -> Response {
                let auth = srv::header_str(&req, &http::header::AUTHORIZATION).unwrap_or("").to_owned();
                let target = srv::header_str(&req, &http::header::HeaderName::from_static("x-amz-target")).unwrap_or("").to_owned();
                let token = srv::header_str(&req, &http::header::HeaderName::from_static("x-amz-security-token")).unwrap_or("").to_owned();
                let body = srv::body_bytes(req.into_body(), 1 << 20).await.unwrap_or_default();
                let body = String::from_utf8_lossy(&body).into_owned();
                if body.contains("Action=AssumeRoleWithWebIdentity") {
                    if !body.contains("WebIdentityToken=good") {
                        let e = "<ErrorResponse><Error><Code>InvalidIdentityToken</Code><Message>No OpenIDConnect provider found</Message></Error></ErrorResponse>";
                        return srv::text(http::StatusCode::BAD_REQUEST, e);
                    }
                    let ok = "<AssumeRoleWithWebIdentityResponse><AssumeRoleWithWebIdentityResult><Credentials><AccessKeyId>ASIAFAKE</AccessKeyId><SecretAccessKey>s3cret</SecretAccessKey><SessionToken>sess</SessionToken><Expiration>2030-01-01T00:00:00Z</Expiration></Credentials></AssumeRoleWithWebIdentityResult></AssumeRoleWithWebIdentityResponse>";
                    return srv::text(http::StatusCode::OK, ok);
                }
                if !auth.starts_with("AWS4-HMAC-SHA256 Credential=ASIAFAKE/") || !auth.contains("/us-east-1/signtest/aws4_request") || !auth.contains("x-amz-security-token") || token != "sess" {
                    return srv::json(http::StatusCode::FORBIDDEN, &serde_json::json!({ "__type": "UnrecognizedClientException", "message": format!("bad signature: {auth}") }));
                }
                match target.as_str() {
                    "AmazonEC2ContainerServiceV20141113.DescribeClusters" => srv::json(http::StatusCode::OK, &serde_json::json!({ "clusters": [{ "clusterName": "main" }], "echo": body })),
                    _ => srv::json(http::StatusCode::BAD_REQUEST, &serde_json::json!({ "__type": "com.amazonaws.ecs#ClientException", "message": "unknown operation" })),
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
            let err = assume_role_with_web_identity(&base, "arn:aws:iam::1:role/kiln", "kiln-test", "bad", 900).await.err().ok_or_else(|| anyhow::anyhow!("a bad token was accepted"))?;
            assert_eq!(err.downcast_ref::<AwsError>().map(|a| a.code.as_str()), Some("InvalidIdentityToken"));
            assert!(advice(&err).is_some_and(|a| a.contains("OpenID Connect provider")));
            let creds = assume_role_with_web_identity(&base, "arn:aws:iam::1:role/kiln", "kiln-test", "good", 900).await?;
            assert_eq!((creds.access_key_id.as_str(), creds.session_token.as_deref(), creds.expires_at), ("ASIAFAKE", Some("sess"), Some(1_893_456_000)));
            assert!(!format!("{creds:?}").contains("s3cret"));
            // A service of its own: overrides are global, and other tests fake ECS.
            override_endpoint("signtest", &base);
            let aws = Aws { creds, region: "us-east-1".into() };
            let v = aws.json("signtest", "AmazonEC2ContainerServiceV20141113.DescribeClusters", &serde_json::json!({ "clusters": ["main"] })).await?;
            assert_eq!(v.pointer("/clusters/0/clusterName").and_then(Value::as_str), Some("main"));
            assert_eq!(v.get("echo").and_then(Value::as_str), Some(r#"{"clusters":["main"]}"#));
            let err = aws.json("signtest", "AmazonEC2ContainerServiceV20141113.Nope", &serde_json::json!({})).await.err().ok_or_else(|| anyhow::anyhow!("an unknown operation answered"))?;
            assert_eq!(err.downcast_ref::<AwsError>().map(|a| (a.code.as_str(), a.message.as_str())), Some(("ClientException", "unknown operation")));
            Ok(())
        })
    }
}
