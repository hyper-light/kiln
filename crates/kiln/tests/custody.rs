//! W27-R08 (docs/ci.md; the Transfers plan's step 2, kiln plan
//! `01a17856b1b477eda14a574ae312a123`): custody records taken over the API, kept, walked and
//! listed, through the API and the CLI. The owner hands a workspace to kiln's server, which passes
//! it on to sam; kiln takes an image for the owner, who can't sign, and hands it to sam; an offer
//! sam never takes stays open, its giver holding it; a receipt of something other than what was
//! handed on breaks its chain, and the break is recorded; a record sam signs once their
//! certificate is revoked is refused, and recorded naming them.
//!
//! `cargo test -p kiln --test custody`

#![cfg(unix)]

mod common;

use std::collections::BTreeMap;
use std::path::Path;
use std::time::Duration;

use common::{Options, http, start};
use kiln_ci::identity::chain::Identity;
use kiln_ci::identity::custody::{Body, Kind, to_value};
use kiln_ci::identity::statement::{PAYLOAD_CUSTODY, Statement, Subject, bundle, digest, payload, to_sign};
use rcgen::SigningKey as _;
use serde_json::{Value, json};

/// A custodian: a key made here, and the certificate kiln's CA issued for it.
struct Signer {
    uri: String,
    key: rcgen::KeyPair,
    der: Vec<u8>,
}

impl Signer {
    fn from_pem(uri: &str, key: &str, cert: &str) -> anyhow::Result<Self> {
        Ok(Self { uri: uri.to_owned(), key: rcgen::KeyPair::from_pem(key)?, der: kiln::pki::der_of(cert)? })
    }

    /// `s`'s payload as this custodian signs it: its body naming it.
    fn payload(&self, s: &Statement) -> anyhow::Result<Vec<u8>> {
        let mut s = s.clone();
        if let Some(o) = s.body.as_object_mut() {
            o.insert("by".into(), Value::String(self.uri.clone()));
        }
        Ok(payload(&s)?)
    }

    fn digest(&self, s: &Statement) -> anyhow::Result<String> {
        Ok(digest(PAYLOAD_CUSTODY, &self.payload(s)?))
    }

    fn sign(&self, s: &Statement) -> anyhow::Result<Value> {
        let p = self.payload(s)?;
        Ok(bundle(PAYLOAD_CUSTODY, &p, &self.key.sign(&to_sign(PAYLOAD_CUSTODY, &p))?, &self.der))
    }
}

/// How a test signs in: the owner's admin token, or a person's session.
#[derive(Clone, Copy)]
enum Auth<'a> {
    Token(&'a str),
    Cookie(&'a str),
}

/// `method path` as whoever `auth` is: its status and body.
fn ask(port: u16, method: &str, path: &str, auth: Auth<'_>, body: Option<&Value>) -> anyhow::Result<(u16, String)> {
    match auth {
        Auth::Token(t) => http(port, method, path, t, body),
        Auth::Cookie(c) => call(port, method, path, Some(c), body).map(|(st, b, _)| (st, b)),
    }
}

fn call(port: u16, method: &str, path: &str, cookie: Option<&str>, body: Option<&Value>) -> anyhow::Result<(u16, String, String)> {
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
    let body = if head.to_ascii_lowercase().contains("transfer-encoding: chunked") { common::dechunk(rest) } else { rest.to_owned() };
    Ok((status, body, head.to_owned()))
}

fn field<'a>(v: &'a Value, k: &str) -> &'a str {
    v.get(k).and_then(Value::as_str).unwrap_or_default()
}

/// A key of this test's, certified by the server for whoever `auth` signs in as.
fn certified(port: u16, auth: Auth<'_>) -> anyhow::Result<Signer> {
    let key = kiln::pki::new_key_pem()?;
    let csr = kiln::pki::make_csr_with_key("kiln-identity", &key)?;
    let (st, body) = ask(port, "POST", "/api/me/identity", auth, Some(&json!({ "csr": csr })))?;
    anyhow::ensure!(st == 200, "certifying a person's key: {st} {body}");
    let v: Value = serde_json::from_str(&body)?;
    Signer::from_pem(field(&v, "uri"), &key, field(&v, "cert_pem"))
}

/// Person `name` added by the owner, signed in in a browser with the link the owner made: their
/// session's cookie.
fn person(port: u16, admin: &str, name: &str) -> anyhow::Result<String> {
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

fn thing(name: &str, c: char) -> Subject {
    Subject { name: name.into(), digest: BTreeMap::from([("sha256".to_owned(), c.to_string().repeat(64))]), ..Subject::default() }
}

/// A custody record of `kind` about `what`, to or from someone, following `follows`, numbered
/// `seq` on its signer's stream `stream`.
fn record(kind: Kind, what: &Subject, to: Option<&str>, from: Option<&str>, follows: &[&str], stream: &str, seq: u64) -> Statement {
    let b = Body { to: to.map(str::to_owned), from: from.map(str::to_owned), why: "a test".into(), ..Body::new(kind, "") };
    Statement { subject: vec![what.clone()], follows: follows.iter().map(|d| (*d).to_owned()).collect(), stream: Some(stream.into()), seq: Some(seq), ..Statement::new(to_value(&b)) }
}

/// `POST /api/custody` of `signed` as `auth`: its status and answer.
fn hand(port: u16, auth: Auth<'_>, signed: &Value) -> anyhow::Result<(u16, Value)> {
    let (st, body) = ask(port, "POST", "/api/custody", auth, Some(signed))?;
    Ok((st, serde_json::from_str(&body).unwrap_or_else(|_| json!({ "body": body }))))
}

/// `GET /api/custody?digest=…`: a chain, walked now.
fn walked(port: u16, admin: &str, digest: &str) -> anyhow::Result<Value> {
    let (st, body) = http(port, "GET", &format!("/api/custody?digest={digest}"), admin, None)?;
    anyhow::ensure!(st == 200, "the chain from {digest}: {st} {body}");
    Ok(serde_json::from_str(&body)?)
}

/// `kiln <args>` against the test server, as the owner: what it printed.
fn cli(port: u16, admin: &str, args: &[&str]) -> anyhow::Result<String> {
    let out = common::kiln().args(args).env("KILN_URL", format!("http://127.0.0.1:{port}")).env("KILN_ADMIN_TOKEN", admin).output()?;
    anyhow::ensure!(out.status.success(), "kiln {args:?}: {}", String::from_utf8_lossy(&out.stderr));
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

/// The hops of a walked chain without both sides named, or without a signer.
fn half_named(v: &Value) -> usize {
    let hops = v.get("hops").and_then(Value::as_array).cloned().unwrap_or_default();
    hops.iter().filter(|h| field(h, "signer").is_empty() || (field(h, "to").is_empty() && field(h, "from").is_empty())).count()
}

#[test]
fn w27_r08_records_through_the_api_and_the_cli() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let s = start(dir.path(), &Options::default())?;
    let (port, admin) = (s.port, s.admin.as_str());
    let owner = certified(port, Auth::Token(admin))?;
    let cookie = person(port, admin, "sam")?;
    let sam = certified(port, Auth::Cookie(&cookie))?;
    let org = Identity::parse(&owner.uri)?.org;
    let server_uri = format!("kiln://{org}/server");
    let pki = dir.path().join("data").join("pki");
    let read = |p: &Path| std::fs::read_to_string(p).map_err(|e| anyhow::anyhow!("{}: {e}", p.display()));
    let kiln = Signer::from_pem(&server_uri, &read(&pki.join("server-signing.key"))?, &read(&pki.join("server-signing.pem"))?)?;

    // The owner hands the workspace to kiln's server, which passes it on to sam, who takes it.
    let ws = thing("workspace tree", 'a');
    let handed = record(Kind::HandedOn, &ws, Some(&server_uri), None, &[], "sync", 1);
    let (st, v) = hand(port, Auth::Token(admin), &owner.sign(&handed)?)?;
    anyhow::ensure!(st == 201, "{st} {v}");
    let h1 = owner.digest(&handed)?;
    assert_eq!(field(&v, "digest"), h1);
    let passed = Statement { body: to_value(&Body { to: Some(sam.uri.clone()), from: Some(owner.uri.clone()), why: "relayed".into(), ..Body::new(Kind::PassedOn, "") }), ..record(Kind::PassedOn, &ws, None, None, &[&h1], "relay", 1) };
    let (st, v) = hand(port, Auth::Token(admin), &kiln.sign(&passed)?)?;
    anyhow::ensure!(st == 201 && v.get("broke") == Some(&Value::Null), "{st} {v}");
    let p1 = kiln.digest(&passed)?;
    let taken = record(Kind::Received, &ws, None, Some(&server_uri), &[&p1], "sync", 1);
    let (st, v) = hand(port, Auth::Cookie(&cookie), &sam.sign(&taken)?)?;
    anyhow::ensure!(st == 201 && v.get("broke") == Some(&Value::Null), "{st} {v}");
    let r1 = sam.digest(&taken)?;
    // Handed again, it's the same record.
    let (st, v) = hand(port, Auth::Cookie(&cookie), &sam.sign(&taken)?)?;
    assert_eq!((st, v.get("again")), (200, Some(&json!(true))), "{v}");

    // The chain, walked from sam's receipt and from the workspace's own digest.
    let chain = walked(port, admin, &r1)?;
    assert_eq!(chain.get("unbroken"), Some(&json!(true)), "{chain}");
    let custodians: Vec<&str> = chain.get("hops").and_then(Value::as_array).map(|h| h.iter().map(|h| field(h, "custodian")).collect()).unwrap_or_default();
    assert_eq!(custodians, vec![sam.uri.as_str(), server_uri.as_str(), owner.uri.as_str()]);
    assert_eq!(field(&chain, "held_by"), sam.uri);
    assert_eq!(field(&walked(port, admin, &format!("sha256:{}", "a".repeat(64)))?, "from"), r1);
    let (st, _) = http(port, "GET", "/api/custody?digest=sha256:feed", admin, None)?;
    assert_eq!(st, 404);

    // kiln takes an image for the owner, who can't sign now, and hands it to sam.
    let img = thing("image kiln:4c1e2a", 'e');
    let land = record(Kind::HandedOn, &img, Some(&owner.uri), None, &[], "land", 1);
    hand(port, Auth::Token(admin), &kiln.sign(&land)?)?;
    let held = record(Kind::Received, &img, None, Some(&server_uri), &[&kiln.digest(&land)?], "sync", 2);
    hand(port, Auth::Token(admin), &owner.sign(&held)?)?;
    let take = Statement {
        body: to_value(&Body { to: Some(sam.uri.clone()), taken_for: Some(owner.uri.clone()), why: "the owner's machine is away".into(), ..Body::new(Kind::Taken, "") }),
        ..record(Kind::Taken, &img, None, None, &[&owner.digest(&held)?], "land", 2)
    };
    hand(port, Auth::Token(admin), &kiln.sign(&take)?)?;
    let got = record(Kind::Received, &img, None, Some(&owner.uri), &[&kiln.digest(&take)?], "sync", 2);
    let (st, v) = hand(port, Auth::Cookie(&cookie), &sam.sign(&got)?)?;
    anyhow::ensure!(st == 201 && v.get("broke") == Some(&Value::Null), "{st} {v}");
    let image_chain = walked(port, admin, &sam.digest(&got)?)?;
    assert_eq!(image_chain.get("unbroken"), Some(&json!(true)), "{image_chain}");
    let kinds: Vec<&str> = image_chain.get("hops").and_then(Value::as_array).map(|h| h.iter().map(|h| field(h, "kind")).collect()).unwrap_or_default();
    assert_eq!(kinds, vec!["received", "taken", "received", "handed-on"]);
    // Every hop of both chains names both its sides and who signed it.
    assert_eq!(half_named(&chain) + half_named(&image_chain), 0);

    // An offer sam never takes: open, the owner still holding it.
    let offer = record(Kind::HandedOn, &thing("report junit.xml", '7'), Some(&sam.uri), None, &[], "sync", 3);
    hand(port, Auth::Token(admin), &owner.sign(&offer)?)?;
    let open = walked(port, admin, &owner.digest(&offer)?)?;
    assert_eq!((open.get("open"), open.get("lapsed"), field(&open, "held_by")), (Some(&json!(true)), Some(&json!(false)), owner.uri.as_str()), "{open}");

    // sam says they received something other than what was passed on: kept, its break recorded.
    let other = record(Kind::Received, &thing("workspace tree", 'b'), None, Some(&server_uri), &[&p1], "sync", 3);
    let (st, v) = hand(port, Auth::Cookie(&cookie), &sam.sign(&other)?)?;
    assert_eq!(st, 201);
    assert!(field(&v, "broke").contains("changed between"), "{v}");
    let (st, body) = http(port, "GET", "/api/custody/breaks", admin, None)?;
    anyhow::ensure!(st == 200, "{st} {body}");
    let breaks: Value = serde_json::from_str(&body)?;
    assert!(breaks.get("breaks").and_then(Value::as_array).is_some_and(|b| b.iter().any(|b| field(b, "kind") == "changed" && field(b, "record") == sam.digest(&other).unwrap_or_default())), "{breaks}");

    // sam's certificate revoked: what they sign after is refused, and recorded naming them.
    let (st, body) = http(port, "POST", "/api/identities/revoke", admin, Some(&json!({ "subject": sam.uri, "reason": "sam's laptop lost" })))?;
    anyhow::ensure!(st == 200, "{st} {body}");
    let late = record(Kind::HandedOn, &thing("goal met", 'd'), Some(&owner.uri), None, &[], "sync", 4);
    let (st, v) = hand(port, Auth::Cookie(&cookie), &sam.sign(&late)?)?;
    assert_eq!(st, 422, "{v}");
    let (st, body) = http(port, "GET", "/api/transfers/refused", admin, None)?;
    anyhow::ensure!(st == 200, "{st} {body}");
    let refused: Value = serde_json::from_str(&body)?;
    let first = refused.get("refused").and_then(Value::as_array).and_then(|r| r.first()).cloned().unwrap_or_default();
    assert_eq!((field(&first, "signer"), field(&first, "digest")), (sam.uri.as_str(), sam.digest(&late)?.as_str()), "{refused}");
    assert!(field(&first, "why").contains("sam's laptop lost"), "{refused}");
    assert!(field(&first, "came_from").starts_with("api:") && field(&first, "came_from").contains("sam"), "{refused}");

    // The CLI says the same.
    let shown = cli(port, admin, &["custody", "show", &r1])?;
    assert!(shown.starts_with("unbroken: 3 hop(s)"), "{shown}");
    assert!(shown.contains(&sam.uri) || shown.contains("sam of"), "{shown}");
    assert!(cli(port, admin, &["custody", "breaks"])?.contains("changed"));
    assert!(cli(port, admin, &["transfers", "refused"])?.contains(&sam.uri));
    Ok(())
}
