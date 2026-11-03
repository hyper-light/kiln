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

/// Thing `thing`'s chain walked as `kiln custody walk` walks it (W27-R08; the Transfers plan's step
/// 15): unbroken, each hop said in words, and its export's records each checking against kiln's
/// CA certificate alone. Then one hop's record (the second the walk read, which the first follows)
/// removed from the server's store (`db`), and then changed there: each walk names the break at
/// that hop, and once it's put back the chain walks again. The first walk.
pub fn walked_and_broken(port: u16, admin: &str, db: &std::path::Path, thing: &str) -> anyhow::Result<Value> {
    use base64::Engine as _;
    let walk = |export: bool| -> anyhow::Result<Value> {
        let (st, body) = http(port, "GET", &format!("/api/custody/walk?digest={thing}{}", if export { "&export=true" } else { "" }), admin, None)?;
        anyhow::ensure!(st == 200, "the walk from {thing}: {st} {body}");
        Ok(serde_json::from_str(&body)?)
    };
    let first = walk(true)?;
    anyhow::ensure!(first.get("unbroken") == Some(&json!(true)), "{thing}: {first:#}");
    let hops = first.get("hops").and_then(Value::as_array).cloned().unwrap_or_default();
    anyhow::ensure!(hops.len() >= 2 && hops.iter().all(|h| !field(h, "words").is_empty()), "each hop in words: {first:#}");
    let ca = kiln::pki::der_of(field(&first, "root"))?;
    let at = kiln::util::now_unix();
    let records = first.get("records").and_then(Value::as_array).cloned().unwrap_or_default();
    anyhow::ensure!(records.len() >= hops.len(), "every record the walk read goes with its export: {first:#}");
    for r in &records {
        let c = kiln_ci::identity::statement::verify(r.get("bundle").unwrap_or(&Value::Null), &ca, &[], at).map_err(|e| anyhow::anyhow!("{}: {e}", field(r, "digest")))?;
        anyhow::ensure!(c.digest == field(r, "digest"), "{} is kept as {}", c.digest, field(r, "digest"));
    }
    let hop = hops.get(1).map(|h| field(h, "record").to_owned()).unwrap_or_default();
    let store = rusqlite::Connection::open(db)?;
    store.busy_timeout(Duration::from_secs(30))?;
    type Row = (String, String, Option<String>, String, Option<String>, Option<String>, Option<i64>, Option<String>, String, String, i64);
    let row: Row = store.query_row(
        "SELECT digest, payload_type, kind, custodian, counterpart, stream, seq, under, envelope, came_from, taken_at FROM custody WHERE digest = ?1",
        [&hop],
        |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?, r.get(5)?, r.get(6)?, r.get(7)?, r.get(8)?, r.get(9)?, r.get(10)?)),
    )?;
    // The break a walk names, once one does.
    let broken = |what: &str| -> anyhow::Result<Value> {
        let v = walk(false)?;
        anyhow::ensure!(v.get("unbroken") == Some(&json!(false)), "{thing} with {hop} {what}: {v:#}");
        Ok(v.get("break").cloned().unwrap_or_default())
    };
    // Removed: the walk breaks where the record before it in the walk follows it, naming it.
    store.execute("DELETE FROM custody WHERE digest = ?1", [&hop])?;
    let b = broken("removed")?;
    anyhow::ensure!(field(&b, "kind") == "no-record" && field(&b, "detail").contains(&hop), "{b:#}");
    store.execute(
        "INSERT INTO custody (digest, payload_type, kind, custodian, counterpart, stream, seq, under, envelope, came_from, taken_at) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)",
        rusqlite::params![row.0, row.1, row.2, row.3, row.4, row.5, row.6, row.7, row.8, row.9, row.10],
    )?;
    anyhow::ensure!(walk(false)?.get("unbroken") == Some(&json!(true)), "{thing} with {hop} put back");
    // Changed where kiln keeps it (its signed payload edited): a break at that record.
    let mut changed: Value = serde_json::from_str(&row.8)?;
    if let Some(p) = changed.pointer_mut("/dsseEnvelope/payload") {
        let was = base64::engine::general_purpose::STANDARD.decode(p.as_str().unwrap_or_default())?;
        let mut now = was.clone();
        if let Some(b) = now.iter_mut().rev().find(|b| b.is_ascii_alphanumeric()) {
            *b = if *b == b'0' { b'1' } else { b'0' };
        }
        *p = Value::String(base64::engine::general_purpose::STANDARD.encode(now));
    }
    store.execute("UPDATE custody SET envelope = ?2 WHERE digest = ?1", rusqlite::params![hop, changed.to_string()])?;
    let b = broken("changed")?;
    anyhow::ensure!(field(&b, "kind") == "unreadable" && field(&b, "record") == hop, "{b:#}");
    store.execute("UPDATE custody SET envelope = ?2 WHERE digest = ?1", rusqlite::params![hop, row.8])?;
    anyhow::ensure!(walk(false)?.get("unbroken") == Some(&json!(true)), "{thing} with {hop} as it was");
    Ok(first)
}
