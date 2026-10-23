//! W10 (docs/ci.md): passkeys and people's own passwords, end to end. A software authenticator
//! (a P-256 key) makes a passkey through the server's WebAuthn ceremony and signs in with it, as a
//! browser's would; the server refuses replays, another origin, a counter gone backwards, another
//! key's signature, a disabled person and a removed passkey. A person sets a password, signs in
//! with their name and it, and changes and removes it with the current one.

#![cfg(unix)]

mod common;

use std::io::{Read as _, Write as _};
use std::net::TcpStream;
use std::time::Duration;

use aws_lc_rs::rand::SystemRandom;
use aws_lc_rs::signature::{ECDSA_P256_SHA256_ASN1_SIGNING, EcdsaKeyPair, KeyPair as _};
use base64::Engine as _;
use common::{Options, Server, http, start};
use serde_json::{Value, json};

/// One request with these headers: its status, headers and body.
fn call(port: u16, method: &str, path: &str, headers: &[(&str, &str)], body: Option<&Value>) -> anyhow::Result<(u16, String, Value)> {
    let mut s = TcpStream::connect(("127.0.0.1", port))?;
    s.set_read_timeout(Some(Duration::from_secs(30)))?;
    let payload = body.map(Value::to_string).unwrap_or_default();
    let extra: String = headers.iter().map(|(k, v)| format!("{k}: {v}\r\n")).collect();
    write!(s, "{method} {path} HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\n{extra}Content-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{payload}", payload.len())?;
    let mut raw = Vec::new();
    s.read_to_end(&mut raw)?;
    let text = String::from_utf8_lossy(&raw).into_owned();
    let (head, rest) = text.split_once("\r\n\r\n").unwrap_or((&text, ""));
    let status: u16 = head.split(' ').nth(1).and_then(|c| c.parse().ok()).unwrap_or(0);
    let rest = if head.to_ascii_lowercase().contains("transfer-encoding: chunked") { common::dechunk(rest) } else { rest.to_owned() };
    Ok((status, head.to_owned(), serde_json::from_str(&rest).unwrap_or(Value::String(rest))))
}

/// The session a response sets: `kiln_session=…`.
fn session(head: &str) -> Option<String> {
    head.lines().find_map(|l| l.to_ascii_lowercase().starts_with("set-cookie:").then(|| l.split_once(':').map(|(_, v)| v.trim().split(';').next().unwrap_or_default().to_owned())).flatten())
}

fn b64(b: &[u8]) -> String {
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(b)
}

fn sha256(b: &[u8]) -> Vec<u8> {
    aws_lc_rs::digest::digest(&aws_lc_rs::digest::SHA256, b).as_ref().to_vec()
}

// CBOR, as much as an authenticator writes.
fn head(major: u8, n: u64) -> Vec<u8> {
    match n {
        0..=23 => vec![(major << 5) | n as u8],
        24..=255 => vec![(major << 5) | 24, n as u8],
        256..=65_535 => [vec![(major << 5) | 25], (n as u16).to_be_bytes().to_vec()].concat(),
        _ => [vec![(major << 5) | 26], (n as u32).to_be_bytes().to_vec()].concat(),
    }
}
fn bytes(b: &[u8]) -> Vec<u8> {
    [head(2, b.len() as u64), b.to_vec()].concat()
}
fn text(t: &str) -> Vec<u8> {
    [head(3, t.len() as u64), t.as_bytes().to_vec()].concat()
}
fn int(i: i64) -> Vec<u8> {
    if i >= 0 { head(0, i as u64) } else { head(1, (-1 - i) as u64) }
}
fn map(pairs: &[(Vec<u8>, Vec<u8>)]) -> Vec<u8> {
    let mut v = head(5, pairs.len() as u64);
    for (k, x) in pairs {
        v.extend(k);
        v.extend(x);
    }
    v
}

/// A software authenticator holding one passkey.
struct Authenticator {
    pair: EcdsaKeyPair,
    id: Vec<u8>,
}

impl Authenticator {
    fn new() -> anyhow::Result<Self> {
        let pair = EcdsaKeyPair::generate(&ECDSA_P256_SHA256_ASN1_SIGNING).map_err(|e| anyhow::anyhow!("{e:?}"))?;
        Ok(Authenticator { pair, id: rand::random::<[u8; 16]>().to_vec() })
    }

    /// What `navigator.credentials.create` answers to `options` on `origin`.
    fn create(&self, options: &Value, origin: &str) -> anyhow::Result<Value> {
        let rp = options.pointer("/rp/id").and_then(Value::as_str).unwrap_or_default();
        let point = self.pair.public_key().as_ref().to_vec();
        let (x, y) = (point.get(1..33).unwrap_or_default(), point.get(33..65).unwrap_or_default());
        let key = map(&[(int(1), int(2)), (int(3), int(-7)), (int(-1), int(1)), (int(-2), bytes(x)), (int(-3), bytes(y))]);
        let len = u16::try_from(self.id.len())?.to_be_bytes();
        let auth = [sha256(rp.as_bytes()), vec![0x45], 0u32.to_be_bytes().to_vec(), vec![0; 16], len.to_vec(), self.id.clone(), key].concat();
        let att = map(&[(text("fmt"), text("none")), (text("attStmt"), map(&[])), (text("authData"), bytes(&auth))]);
        let client = json!({ "type": "webauthn.create", "challenge": options.get("challenge"), "origin": origin, "crossOrigin": false }).to_string();
        Ok(json!({ "name": "laptop", "id": b64(&self.id), "clientDataJSON": b64(client.as_bytes()), "attestationObject": b64(&att), "transports": ["internal"] }))
    }

    /// What `navigator.credentials.get` answers to `options` on `origin`, its counter at `count`.
    fn get(&self, options: &Value, origin: &str, count: u32, handle: &str) -> anyhow::Result<Value> {
        let rp = options.get("rpId").and_then(Value::as_str).unwrap_or_default();
        let auth = [sha256(rp.as_bytes()), vec![0x05], count.to_be_bytes().to_vec()].concat();
        let client = json!({ "type": "webauthn.get", "challenge": options.get("challenge"), "origin": origin }).to_string();
        let signed = [auth.clone(), sha256(client.as_bytes())].concat();
        let sig = self.pair.sign(&SystemRandom::new(), &signed).map_err(|e| anyhow::anyhow!("{e:?}"))?;
        Ok(json!({ "id": b64(&self.id), "clientDataJSON": b64(client.as_bytes()), "authenticatorData": b64(&auth), "signature": b64(sig.as_ref()), "userHandle": b64(handle.as_bytes()) }))
    }
}

/// A person's sign-in session, by their link.
fn signed_in(s: &Server, name: &str) -> anyhow::Result<String> {
    let (st, body) = http(s.port, "POST", &format!("/api/users/{name}/link"), &s.admin, None)?;
    anyhow::ensure!(st == 200, "link: {st} {body}");
    let url = serde_json::from_str::<Value>(&body)?.get("url").and_then(Value::as_str).unwrap_or_default().to_owned();
    let path = url.split_once("://").and_then(|(_, rest)| rest.find('/').map(|i| rest.get(i..).unwrap_or_default().to_owned())).unwrap_or_default();
    let (_, h, _) = call(s.port, "GET", &path, &[], None)?;
    session(&h).ok_or_else(|| anyhow::anyhow!("no session from {path}: {h}"))
}

#[test]
fn w10_passkeys_and_passwords() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let s = start(dir.path(), &Options::default())?;
    let origin = format!("http://127.0.0.1:{}", s.port);
    let (st, body) = http(s.port, "POST", "/api/users", &s.admin, Some(&json!({ "name": "ada", "grants": ["viewer"] })))?;
    anyhow::ensure!((200..300).contains(&st), "ada: {st} {body}");
    let ada = signed_in(&s, "ada")?;
    let me = |cookie: &str| -> anyhow::Result<Value> { Ok(call(s.port, "GET", "/api/me", &[("Cookie", cookie)], None)?.2) };
    let ada_id = me(&ada)?.get("principal").and_then(Value::as_str).map(str::to_owned).unwrap_or_default();
    assert_eq!(ada_id, "user:ada");
    let as_ada = [("Cookie", ada.as_str()), ("Origin", origin.as_str())];

    // Making a passkey: options for this domain, the authenticator's answer, kept.
    let (st, _, options) = call(s.port, "POST", "/api/me/passkeys/options", &as_ada, Some(&json!({})))?;
    assert_eq!((st, options.pointer("/rp/id")), (200, Some(&json!("127.0.0.1"))), "{options}");
    assert_eq!(options.pointer("/authenticatorSelection/userVerification"), Some(&json!("required")));
    let handle = String::from_utf8(base64::engine::general_purpose::URL_SAFE_NO_PAD.decode(options.pointer("/user/id").and_then(Value::as_str).unwrap_or_default())?)?;
    let key = Authenticator::new()?;
    let made = key.create(&options, &origin)?;
    let (st, _, kept) = call(s.port, "POST", "/api/me/passkeys", &as_ada, Some(&made))?;
    assert_eq!(st, 201, "{kept}");
    let (st, _, again) = call(s.port, "POST", "/api/me/passkeys", &as_ada, Some(&made))?;
    assert_eq!(st, 401, "a challenge is used once: {again}");
    let (_, _, list) = call(s.port, "GET", "/api/me/passkeys", &as_ada, None)?;
    assert_eq!(list.pointer("/passkeys/0/name"), Some(&json!("laptop")), "{list}");
    let (_, _, methods) = call(s.port, "GET", "/api/auth/methods", &[], None)?;
    assert_eq!(methods.get("passkey"), Some(&json!(true)), "{methods}");

    // Signing in with it, from nothing.
    let anyone = [("Origin", origin.as_str())];
    let sign_in = |count: u32, by: &Authenticator| -> anyhow::Result<(u16, String, Value)> {
        let (st, _, o) = call(s.port, "POST", "/api/auth/passkey/options", &anyone, Some(&json!({})))?;
        anyhow::ensure!(st == 200, "options: {st} {o}");
        call(s.port, "POST", "/api/auth/passkey", &anyone, Some(&by.get(&o, &origin, count, &handle)?))
    };
    let (st, h, answer) = sign_in(1, &key)?;
    assert_eq!(st, 204, "{answer}");
    let cookie = session(&h).ok_or_else(|| anyhow::anyhow!("no session: {h}"))?;
    assert_eq!(me(&cookie)?.get("principal"), Some(&json!("user:ada")));

    // Refused: the same answer again, a counter gone backwards, another key, another origin.
    let (st, _, o) = call(s.port, "POST", "/api/auth/passkey/options", &anyone, Some(&json!({})))?;
    anyhow::ensure!(st == 200, "{o}");
    let answer = key.get(&o, &origin, 2, &handle)?;
    assert_eq!(call(s.port, "POST", "/api/auth/passkey", &anyone, Some(&answer))?.0, 204);
    assert_eq!(call(s.port, "POST", "/api/auth/passkey", &anyone, Some(&answer))?.0, 401, "a replay");
    let (st, _, why) = sign_in(2, &key)?;
    assert!(st == 401 && why.to_string().contains("backwards"), "{st} {why}");
    let other = Authenticator { id: key.id.clone(), ..Authenticator::new()? };
    let (st, _, why) = sign_in(9, &other)?;
    assert!(st == 401 && why.to_string().contains("signature"), "{st} {why}");
    let (st, _, why) = call(s.port, "POST", "/api/auth/passkey/options", &[("Origin", "https://evil.example")], Some(&json!({})))?;
    assert_eq!(st, 400, "another site's origin: {why}");

    // The owner's own.
    let as_owner = [("Authorization", format!("Bearer {}", s.admin)), ("Origin", origin.clone())];
    let as_owner: Vec<(&str, &str)> = as_owner.iter().map(|(k, v)| (*k, v.as_str())).collect();
    let (_, _, owner_options) = call(s.port, "POST", "/api/me/passkeys/options", &as_owner, Some(&json!({})))?;
    let owner_key = Authenticator::new()?;
    let (st, _, kept) = call(s.port, "POST", "/api/me/passkeys", &as_owner, Some(&owner_key.create(&owner_options, &origin)?))?;
    assert_eq!(st, 201, "{kept}");
    let (_, _, o) = call(s.port, "POST", "/api/auth/passkey/options", &anyone, Some(&json!({})))?;
    let (st, h, _) = call(s.port, "POST", "/api/auth/passkey", &anyone, Some(&owner_key.get(&o, &origin, 0, "owner")?))?;
    assert_eq!(st, 204);
    assert_eq!(me(&session(&h).unwrap_or_default())?.get("kind"), Some(&json!("owner")));

    // A token is no person's session: it makes no passkey and sets no password.
    let (st, body) = http(s.port, "POST", "/api/tokens", &s.admin, Some(&json!({ "kind": "pat", "user": "ada", "grants": ["viewer"] })))?;
    anyhow::ensure!((200..300).contains(&st), "{body}");
    let pat = serde_json::from_str::<Value>(&body)?.get("token").and_then(Value::as_str).unwrap_or_default().to_owned();
    assert_eq!(http(s.port, "POST", "/api/me/passkeys/options", &pat, Some(&json!({})))?.0, 403);
    assert_eq!(http(s.port, "PUT", "/api/me/password", &pat, Some(&json!({ "password": "a long passphrase" })))?.0, 403);

    // A password of ada's own: set, signed in with by name, changed and removed with the current.
    let (st, _, set) = call(s.port, "PUT", "/api/me/password", &as_ada, Some(&json!({ "password": "correct horse battery" })))?;
    assert_eq!((st, set.get("set")), (200, Some(&json!(true))), "{set}");
    assert_eq!(call(s.port, "GET", "/api/auth/methods", &[], None)?.2.get("people"), Some(&json!(true)));
    let by_name = |name: &str, password: &str| call(s.port, "POST", "/api/auth/password", &[], Some(&json!({ "name": name, "password": password })));
    assert_eq!(by_name("ada", "wrong horse")?.0, 401);
    assert_eq!(by_name("nobody", "correct horse battery")?.0, 401);
    let (st, h, _) = by_name("ADA", "correct horse battery")?;
    assert_eq!(st, 204);
    assert_eq!(me(&session(&h).unwrap_or_default())?.get("principal"), Some(&json!("user:ada")));
    let (st, _, why) = call(s.port, "PUT", "/api/me/password", &as_ada, Some(&json!({ "password": "another long one" })))?;
    assert_eq!(st, 403, "changing it takes the current one: {why}");
    let (st, _, set) = call(s.port, "PUT", "/api/me/password", &as_ada, Some(&json!({ "password": "", "current": "correct horse battery" })))?;
    assert_eq!((st, set.get("set")), (200, Some(&json!(false))), "{set}");
    assert_eq!(by_name("ada", "correct horse battery")?.0, 401, "removed");

    // A passkey removed, and a person disabled, sign in no more.
    let (st, _, _) = call(s.port, "DELETE", &format!("/api/me/passkeys/{}", b64(&owner_key.id)), &as_owner, None)?;
    assert_eq!(st, 204);
    let (_, _, o) = call(s.port, "POST", "/api/auth/passkey/options", &anyone, Some(&json!({})))?;
    assert_eq!(call(s.port, "POST", "/api/auth/passkey", &anyone, Some(&owner_key.get(&o, &origin, 0, "owner")?))?.0, 401, "removed");
    let (st, body) = http(s.port, "DELETE", "/api/users/ada", &s.admin, None)?;
    anyhow::ensure!((200..300).contains(&st), "disable: {st} {body}");
    let (st, _, why) = sign_in(3, &key)?;
    assert!(st == 401 && why.to_string().contains("disabled"), "{st} {why}");
    Ok(())
}
