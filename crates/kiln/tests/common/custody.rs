//! What the custody and transfers tests share (docs/ci.md W27-R08): a key of a test's own that
//! kiln's CA certified (a custodian), signing in as the owner or as a person, and reading a walked
//! chain.

use std::time::Duration;

use kiln_ci::identity::statement::{PAYLOAD_CUSTODY, Statement, bundle, digest, payload, to_sign};
use rcgen::SigningKey as _;
use serde_json::{Value, json};

use super::http;

/// A custodian: a key made here, and the certificate kiln's CA issued for it.
pub struct Signer {
    pub uri: String,
    pub key: rcgen::KeyPair,
    pub der: Vec<u8>,
}

impl Signer {
    pub fn from_pem(uri: &str, key: &str, cert: &str) -> anyhow::Result<Self> {
        Ok(Self { uri: uri.to_owned(), key: rcgen::KeyPair::from_pem(key)?, der: kiln::pki::der_of(cert)? })
    }

    /// `s`'s payload as this custodian signs it: its body naming it.
    pub fn payload(&self, s: &Statement) -> anyhow::Result<Vec<u8>> {
        let mut s = s.clone();
        if let Some(o) = s.body.as_object_mut() {
            o.insert("by".into(), Value::String(self.uri.clone()));
        }
        Ok(payload(&s)?)
    }

    pub fn digest(&self, s: &Statement) -> anyhow::Result<String> {
        Ok(digest(PAYLOAD_CUSTODY, &self.payload(s)?))
    }

    pub fn sign(&self, s: &Statement) -> anyhow::Result<Value> {
        let p = self.payload(s)?;
        Ok(bundle(PAYLOAD_CUSTODY, &p, &self.key.sign(&to_sign(PAYLOAD_CUSTODY, &p))?, &self.der))
    }
}

/// How a test signs in: the owner's admin token, or a person's session.
#[derive(Clone, Copy)]
pub enum Auth<'a> {
    Token(&'a str),
    Cookie(&'a str),
}

/// `method path` as whoever `auth` is: its status and body.
pub fn ask(port: u16, method: &str, path: &str, auth: Auth<'_>, body: Option<&Value>) -> anyhow::Result<(u16, String)> {
    match auth {
        Auth::Token(t) => http(port, method, path, t, body),
        Auth::Cookie(c) => call(port, method, path, Some(c), body).map(|(st, b, _)| (st, b)),
    }
}

pub fn call(port: u16, method: &str, path: &str, cookie: Option<&str>, body: Option<&Value>) -> anyhow::Result<(u16, String, String)> {
    use std::io::{Read as _, Write as _};
    let mut s = std::net::TcpStream::connect(("127.0.0.1", port))?;
    s.set_read_timeout(Some(Duration::from_secs(30)))?;
    let payload = body.map(Value::to_string).unwrap_or_default();
    let cookie = cookie.map(|c| format!("Cookie: kiln_session={c}\r\n")).unwrap_or_default();
    write!(s, "{method} {path} HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\n{cookie}Content-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{payload}", payload.len())?;
    let mut raw = Vec::new();
    s.read_to_end(&mut raw)?;
    let text = String::from_utf8_lossy(&raw).into_owned();
    let (head, rest) = text.split_once("\r\n\r\n").unwrap_or((&text, ""));
    let status: u16 = head.split(' ').nth(1).and_then(|c| c.parse().ok()).unwrap_or(0);
    let body = if head.to_ascii_lowercase().contains("transfer-encoding: chunked") { super::dechunk(rest) } else { rest.to_owned() };
    Ok((status, body, head.to_owned()))
}

pub fn field<'a>(v: &'a Value, k: &str) -> &'a str {
    v.get(k).and_then(Value::as_str).unwrap_or_default()
}

/// A key of this test's, certified by the server for whoever `auth` signs in as.
pub fn certified(port: u16, auth: Auth<'_>) -> anyhow::Result<Signer> {
    let key = kiln::pki::new_key_pem()?;
    let csr = kiln::pki::make_csr_with_key("kiln-identity", &key)?;
    let (st, body) = ask(port, "POST", "/api/me/identity", auth, Some(&json!({ "csr": csr })))?;
    anyhow::ensure!(st == 200, "certifying a person's key: {st} {body}");
    let v: Value = serde_json::from_str(&body)?;
    Signer::from_pem(field(&v, "uri"), &key, field(&v, "cert_pem"))
}

/// Person `name` added by the owner, signed in in a browser with the link the owner made: their
/// session's cookie.
pub fn person(port: u16, admin: &str, name: &str) -> anyhow::Result<String> {
    let (st, body) = http(port, "POST", "/api/users", admin, Some(&json!({ "name": name, "grants": ["developer"] })))?;
    anyhow::ensure!((200..300).contains(&st), "{name}: {st} {body}");
    let (st, body) = http(port, "POST", &format!("/api/users/{name}/link"), admin, None)?;
    anyhow::ensure!(st == 200, "{name}'s link: {st} {body}");
    let url = serde_json::from_str::<Value>(&body)?.get("url").and_then(Value::as_str).map(str::to_owned).ok_or_else(|| anyhow::anyhow!("no link: {body}"))?;
    let path = url.find("/api/").and_then(|at| url.get(at..)).ok_or_else(|| anyhow::anyhow!("a link without a path: {url}"))?;
    let (_, _, head) = call(port, "GET", path, None, None)?;
    head.lines()
        .filter(|l| l.to_ascii_lowercase().starts_with("set-cookie:"))
        .find_map(|l| l.split_once(':').map(|(_, v)| v.trim()).and_then(|v| v.split(';').next()).and_then(|v| v.strip_prefix("kiln_session=")))
        .map(str::to_owned)
        .ok_or_else(|| anyhow::anyhow!("{name}'s sign-in set no session: {head}"))
}

/// The hops of a walked chain without both sides named, or without a signer.
pub fn half_named(v: &Value) -> usize {
    let hops = v.get("hops").and_then(Value::as_array).cloned().unwrap_or_default();
    hops.iter().filter(|h| field(h, "signer").is_empty() || (field(h, "to").is_empty() && field(h, "from").is_empty())).count()
}
