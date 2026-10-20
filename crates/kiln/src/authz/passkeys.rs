//! Passkeys (docs/ci.md W10): people and the owner sign in with a WebAuthn credential made on
//! their phone, computer or password manager, beside sign-in links, SSO and passwords.
//!
//! - **Making one**, signed in: `POST /api/me/passkeys/options` gives the browser what
//!   `navigator.credentials.create` needs (a challenge, this domain as the relying party, a
//!   discoverable credential with user verification), and `POST /api/me/passkeys` checks what it
//!   made and keeps its public key. Attestation isn't asked for: kiln trusts the person signed in
//!   to say which authenticator is theirs.
//! - **Signing in**: `POST /api/auth/passkey/options` gives a challenge, the browser's
//!   `navigator.credentials.get` lets the person pick a passkey of this domain, and
//!   `POST /api/auth/passkey` checks its signature and starts their session.
//! - Each challenge is used once, within 5 minutes, on the origin it was given to. The origins are
//!   the server's public URL and its front doors'; a passkey belongs to the domain it was made on.
//! - ES256, EdDSA and RS256 keys; a signature counter that goes backwards is refused (a cloned
//!   authenticator), one that stays at 0 (synced passkeys) isn't.

use std::collections::HashMap;
use std::time::{Duration, Instant};

use base64::Engine as _;
use http::StatusCode;
use rusqlite::{OptionalExtension as _, params};
use serde_json::{Value, json};

use super::Caller;
use super::gate::Kind;
use crate::hub::Hub;

type Fail = (StatusCode, String);

/// How long a ceremony's challenge lasts.
const CHALLENGE_TTL: Duration = Duration::from_secs(300);
/// At most this many ceremonies under way at once (sign-in options are anyone's to ask for).
const PENDING_MAX: usize = 10_000;
/// At most this many passkeys a person keeps.
const PER_PERSON: i64 = 50;

/// The COSE algorithms kiln verifies, in the order it prefers them.
const ES256: i64 = -7;
const EDDSA: i64 = -8;
const RS256: i64 = -257;

fn bad(m: impl Into<String>) -> Fail {
    (StatusCode::BAD_REQUEST, m.into())
}

fn refused(m: impl Into<String>) -> Fail {
    (StatusCode::UNAUTHORIZED, m.into())
}

fn b64(bytes: &[u8]) -> String {
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes)
}

/// base64url, with or without padding (browsers' `toJSON` leaves it off).
fn unb64(s: &str) -> Option<Vec<u8>> {
    base64::engine::general_purpose::URL_SAFE_NO_PAD.decode(s.trim().trim_end_matches('=')).ok()
}

// ------------------------------------------------------------------ ceremonies under way

/// What a challenge is for.
#[derive(Clone)]
enum Purpose {
    /// Making a passkey for `principal` (`owner`, `user:<id>`).
    Register { principal: String },
    SignIn,
}

#[derive(Clone)]
struct Pending {
    purpose: Purpose,
    origin: String,
    rp_id: String,
    until: Instant,
}

fn pending() -> &'static parking_lot::Mutex<HashMap<String, Pending>> {
    static P: std::sync::OnceLock<parking_lot::Mutex<HashMap<String, Pending>>> = std::sync::OnceLock::new();
    P.get_or_init(Default::default)
}

/// A new challenge for `purpose` on `origin` (whose host is the relying party).
fn challenge(purpose: Purpose, origin: &str, rp_id: &str) -> Result<String, Fail> {
    let c = b64(&rand::random::<[u8; 32]>());
    let mut p = pending().lock();
    let now = Instant::now();
    p.retain(|_, x| x.until > now);
    if p.len() >= PENDING_MAX {
        return Err((StatusCode::SERVICE_UNAVAILABLE, "too many sign-ins under way: try again in a minute".into()));
    }
    p.insert(c.clone(), Pending { purpose, origin: origin.to_owned(), rp_id: rp_id.to_owned(), until: now + CHALLENGE_TTL });
    Ok(c)
}

/// The ceremony `challenge` (base64url) began, taken: each is used once.
fn take(challenge: &str) -> Option<Pending> {
    let key = b64(&unb64(challenge)?);
    pending().lock().remove(&key).filter(|p| p.until > Instant::now())
}

/// The origin a ceremony runs on: the request's, when it's one of the server's (its public URL,
/// its front doors); else the public URL's. Its host is the relying party.
fn origin_of(hub: &Hub, asked: Option<&str>) -> Result<(String, String), Fail> {
    let public = origin(&hub.cfg.server.public_url);
    let mut allowed = vec![public.clone()];
    allowed.extend(hub.front_doors.origins().iter().map(|o| origin(o)));
    let chosen = match asked.map(origin) {
        Some(o) if allowed.contains(&o) => o,
        Some(o) => return Err(bad(format!("passkeys work on this server's own addresses ({}), not {o}", allowed.join(", ")))),
        None => public,
    };
    let host = chosen.split_once("://").map_or(chosen.as_str(), |(_, rest)| rest);
    let host = host.rsplit_once(':').filter(|(_, port)| port.chars().all(|c| c.is_ascii_digit())).map_or(host, |(h, _)| h);
    let rp_id = host.trim_start_matches('[').trim_end_matches(']').to_ascii_lowercase();
    Ok((chosen, rp_id))
}

/// `scheme://host[:port]` of a URL, lowercased, without a default port.
fn origin(url: &str) -> String {
    let url = url.trim();
    let (scheme, rest) = url.split_once("://").unwrap_or(("https", url));
    let host = rest.split(['/', '?', '#']).next().unwrap_or_default().to_ascii_lowercase();
    let host = match (scheme, host.strip_suffix(":443"), host.strip_suffix(":80")) {
        ("https", Some(h), _) | ("http", _, Some(h)) => h.to_owned(),
        _ => host,
    };
    format!("{}://{host}", scheme.to_ascii_lowercase())
}

// ------------------------------------------------------------------ CBOR, as WebAuthn uses it

/// A CBOR item (RFC 8949), as much of CBOR as WebAuthn's objects use.
#[derive(Debug, Clone, PartialEq)]
enum Cbor {
    Int(i128),
    Bytes(Vec<u8>),
    Text(String),
    Array(Vec<Cbor>),
    Map(Vec<(Cbor, Cbor)>),
    Bool(bool),
    Null,
}

impl Cbor {
    /// A map's value at integer key `k` (COSE's labels).
    fn at(&self, k: i128) -> Option<&Cbor> {
        let Cbor::Map(m) = self else { return None };
        m.iter().find(|(key, _)| *key == Cbor::Int(k)).map(|(_, v)| v)
    }

    /// A map's value at text key `k`.
    fn field(&self, k: &str) -> Option<&Cbor> {
        let Cbor::Map(m) = self else { return None };
        m.iter().find(|(key, _)| matches!(key, Cbor::Text(t) if t == k)).map(|(_, v)| v)
    }

    fn int(&self) -> Option<i128> {
        if let Cbor::Int(i) = self { Some(*i) } else { None }
    }

    fn bytes(&self) -> Option<&[u8]> {
        if let Cbor::Bytes(b) = self { Some(b) } else { None }
    }
}

/// The first `n` bytes of `b`, and the rest.
fn split(b: &[u8], n: usize) -> Result<(&[u8], &[u8]), String> {
    if b.len() < n { Err("it ends early".into()) } else { Ok(b.split_at(n)) }
}

/// One CBOR item from the start of `b`: it, and the bytes after it.
fn cbor(b: &[u8], depth: u32) -> Result<(Cbor, &[u8]), String> {
    if depth > 16 {
        return Err("it nests too deep".into());
    }
    let (first, rest) = b.split_first().ok_or("it ends early")?;
    let (major, info) = (first >> 5, first & 0x1f);
    let (arg, rest) = match info {
        0..=23 => (u64::from(info), rest),
        24..=27 => {
            let (n, rest) = split(rest, 1 << (info - 24))?;
            (n.iter().fold(0u64, |acc, x| (acc << 8) | u64::from(*x)), rest)
        }
        _ => return Err("indefinite lengths aren't WebAuthn's".into()),
    };
    let len = |n: u64| usize::try_from(n).ok().filter(|n| *n <= rest.len()).ok_or("a length past its end");
    match major {
        0 => Ok((Cbor::Int(i128::from(arg)), rest)),
        1 => Ok((Cbor::Int(-1 - i128::from(arg)), rest)),
        2 => {
            let (v, r) = split(rest, len(arg)?)?;
            Ok((Cbor::Bytes(v.to_vec()), r))
        }
        3 => {
            let (v, r) = split(rest, len(arg)?)?;
            Ok((Cbor::Text(String::from_utf8(v.to_vec()).map_err(|_| "text that isn't UTF-8")?), r))
        }
        4 => {
            let mut r = rest;
            let mut items = Vec::new();
            for _ in 0..len(arg)? {
                let (v, after) = cbor(r, depth + 1)?;
                items.push(v);
                r = after;
            }
            Ok((Cbor::Array(items), r))
        }
        5 => {
            let mut r = rest;
            let mut pairs = Vec::new();
            for _ in 0..len(arg)? {
                let (k, after) = cbor(r, depth + 1)?;
                let (v, after) = cbor(after, depth + 1)?;
                pairs.push((k, v));
                r = after;
            }
            Ok((Cbor::Map(pairs), r))
        }
        7 => match info {
            20 => Ok((Cbor::Bool(false), rest)),
            21 => Ok((Cbor::Bool(true), rest)),
            22 => Ok((Cbor::Null, rest)),
            _ => Err("a CBOR value WebAuthn doesn't use".into()),
        },
        _ => Err("CBOR tags aren't WebAuthn's".into()),
    }
}

// ------------------------------------------------------------------ keys and signatures

/// A credential's public key.
enum Key {
    /// An uncompressed P-256 point (`04 ‖ x ‖ y`).
    Es256(Vec<u8>),
    Ed25519(Vec<u8>),
    Rs256 { n: Vec<u8>, e: Vec<u8> },
}

/// A COSE key (RFC 9053): its algorithm and key.
fn cose(bytes: &[u8]) -> Result<(i64, Key), String> {
    let (k, rest) = cbor(bytes, 0).map_err(|e| format!("its public key: {e}"))?;
    if !rest.is_empty() {
        return Err("its public key has bytes after it".into());
    }
    let int = |l: i128| k.at(l).and_then(Cbor::int);
    let raw = |l: i128| k.at(l).and_then(Cbor::bytes).map(<[u8]>::to_vec);
    match (int(1), int(3)) {
        // EC2 on P-256.
        (Some(2), Some(-7)) if int(-1) == Some(1) => {
            let (x, y) = (raw(-2).ok_or("an EC2 key without x")?, raw(-3).ok_or("an EC2 key without y")?);
            if x.len() != 32 || y.len() != 32 {
                return Err("a P-256 point of the wrong size".into());
            }
            Ok((ES256, Key::Es256([&[4u8][..], &x, &y].concat())))
        }
        // OKP Ed25519.
        (Some(1), Some(-8)) if int(-1) == Some(6) => {
            let x = raw(-2).filter(|x| x.len() == 32).ok_or("an Ed25519 key of the wrong size")?;
            Ok((EDDSA, Key::Ed25519(x)))
        }
        (Some(3), Some(-257)) => {
            let (n, e) = (raw(-1).ok_or("an RSA key without n")?, raw(-2).ok_or("an RSA key without e")?);
            if n.len() < 256 {
                return Err("an RSA key under 2048 bits".into());
            }
            Ok((RS256, Key::Rs256 { n, e }))
        }
        (kty, alg) => Err(format!("a key kiln doesn't verify (kty {kty:?}, alg {alg:?}): ES256, EdDSA or RS256")),
    }
}

/// Whether `sig` is `key`'s signature of `msg`.
fn verify(key: &Key, msg: &[u8], sig: &[u8]) -> bool {
    use aws_lc_rs::signature::{ECDSA_P256_SHA256_ASN1, ED25519, RSA_PKCS1_2048_8192_SHA256, RsaPublicKeyComponents, UnparsedPublicKey};
    match key {
        Key::Es256(point) => UnparsedPublicKey::new(&ECDSA_P256_SHA256_ASN1, point).verify(msg, sig).is_ok(),
        Key::Ed25519(x) => UnparsedPublicKey::new(&ED25519, x).verify(msg, sig).is_ok(),
        Key::Rs256 { n, e } => RsaPublicKeyComponents { n: n.as_slice(), e: e.as_slice() }.verify(&RSA_PKCS1_2048_8192_SHA256, msg, sig).is_ok(),
    }
}

// ------------------------------------------------------------------ what the authenticator says

/// Authenticator data (WebAuthn §6.1).
struct AuthData {
    rp_id_hash: Vec<u8>,
    flags: u8,
    sign_count: u32,
    /// A credential made now: its id and COSE key.
    made: Option<(Vec<u8>, Vec<u8>)>,
}

const UP: u8 = 0x01;
const UV: u8 = 0x04;
const BS: u8 = 0x10;
const AT: u8 = 0x40;

fn auth_data(b: &[u8]) -> Result<AuthData, String> {
    let (rp, r) = split(b, 32)?;
    let (flags, r) = split(r, 1)?;
    let (count, r) = split(r, 4)?;
    let flags = flags.first().copied().unwrap_or_default();
    let sign_count = u32::from_be_bytes(count.try_into().map_err(|_| "its counter")?);
    let made = if flags & AT != 0 {
        let (_aaguid, r) = split(r, 16)?;
        let (len, r) = split(r, 2)?;
        let len = usize::from(u16::from_be_bytes(len.try_into().map_err(|_| "its credential's length")?));
        let (id, r) = split(r, len)?;
        let (_, after) = cbor(r, 0).map_err(|e| format!("its public key: {e}"))?;
        let key = r.get(..r.len() - after.len()).ok_or("its public key")?;
        Some((id.to_vec(), key.to_vec()))
    } else {
        None
    };
    Ok(AuthData { rp_id_hash: rp.to_vec(), flags, sign_count, made })
}

/// The browser's client data, checked against the ceremony it answers: its type, challenge and
/// origin. The ceremony, taken.
fn client_data(raw: &[u8], kind: &str) -> Result<Pending, Fail> {
    let c: Value = serde_json::from_slice(raw).map_err(|_| bad("clientDataJSON isn't JSON"))?;
    if c.get("type").and_then(Value::as_str) != Some(kind) {
        return Err(bad(format!("clientDataJSON isn't a {kind}")));
    }
    if c.get("crossOrigin").and_then(Value::as_bool) == Some(true) {
        return Err(refused("a passkey made or used inside another site's frame"));
    }
    let p = take(c.get("challenge").and_then(Value::as_str).unwrap_or_default()).ok_or_else(|| refused("that challenge isn't one of this server's, or it's been used or has expired: start again"))?;
    let said = c.get("origin").and_then(Value::as_str).map(origin).unwrap_or_default();
    if said != p.origin {
        return Err(refused(format!("made on {said}, asked on {}", p.origin)));
    }
    Ok(p)
}

fn sha256(b: &[u8]) -> Vec<u8> {
    aws_lc_rs::digest::digest(&aws_lc_rs::digest::SHA256, b).as_ref().to_vec()
}

fn field(body: &Value, k: &str) -> Result<Vec<u8>, Fail> {
    body.get(k).and_then(Value::as_str).and_then(unb64).ok_or_else(|| bad(format!("`{k}` (base64url) is needed")))
}

// ------------------------------------------------------------------ making one

/// Whose passkeys the caller keeps: the owner's, or a person's (`user:<id>`), signed in
/// themselves (not through a token or an agent, whose passkey would outlast it).
fn principal(caller: &Caller) -> Result<String, Fail> {
    if caller.agent.is_some() {
        return Err((StatusCode::FORBIDDEN, "a passkey is a person's own: an agent doesn't make one".into()));
    }
    match (&caller.kind, &caller.user) {
        (Kind::Owner, _) => Ok("owner".into()),
        (Kind::User, Some(id)) if caller.token.is_none() && caller.agent.is_none() => Ok(format!("user:{id}")),
        _ => Err((StatusCode::FORBIDDEN, "a passkey is a person's own: sign in as yourself to make one".into())),
    }
}

/// `POST /api/me/passkeys/options`: what the browser needs to make a passkey for the caller here.
pub async fn register_options(hub: &Hub, caller: &Caller, asked_origin: Option<&str>) -> Result<Value, Fail> {
    let who = principal(caller)?;
    let (origin, rp_id) = origin_of(hub, asked_origin)?;
    let (p, r) = (who.clone(), rp_id.clone());
    let existing: Vec<String> = hub
        .store
        .call(move |c| {
            let mut st = c.prepare("SELECT id FROM passkeys WHERE principal = ?1 AND rp_id = ?2")?;
            let rows = st.query_map(params![p, r], |r| r.get(0))?.collect::<Result<Vec<String>, _>>()?;
            Ok(rows)
        })
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, format!("{e:#}")))?;
    let (handle, name) = match who.strip_prefix("user:") {
        Some(id) => (id.to_owned(), caller.name.trim_start_matches("user:").to_owned()),
        None => ("owner".to_owned(), "owner".to_owned()),
    };
    let c = challenge(Purpose::Register { principal: who }, &origin, &rp_id)?;
    let algs: Vec<Value> = [ES256, EDDSA, RS256].iter().map(|a| json!({ "type": "public-key", "alg": a })).collect();
    let exclude: Vec<Value> = existing.iter().map(|id| json!({ "type": "public-key", "id": id })).collect();
    Ok(json!({
        "challenge": c,
        "rp": { "id": rp_id, "name": format!("kiln ({rp_id})") },
        "user": { "id": b64(handle.as_bytes()), "name": name, "displayName": name },
        "pubKeyCredParams": algs,
        "timeout": CHALLENGE_TTL.as_millis(),
        "attestation": "none",
        "authenticatorSelection": { "residentKey": "required", "requireResidentKey": true, "userVerification": "required" },
        "excludeCredentials": exclude,
    }))
}

/// `POST /api/me/passkeys` `{name, id, clientDataJSON, attestationObject, transports}`: the
/// passkey the browser made, checked and kept.
pub async fn register(hub: &Hub, caller: &Caller, body: &Value) -> Result<Value, Fail> {
    let who = principal(caller)?;
    let name = body.get("name").and_then(Value::as_str).map(str::trim).filter(|n| !n.is_empty()).unwrap_or("A passkey").chars().take(80).collect::<String>();
    let (id, client, att) = (field(body, "id")?, field(body, "clientDataJSON")?, field(body, "attestationObject")?);
    let p = client_data(&client, "webauthn.create")?;
    match &p.purpose {
        Purpose::Register { principal } if *principal == who => {}
        _ => return Err(refused("that challenge was for someone else, or for signing in")),
    }
    let (obj, rest) = cbor(&att, 0).map_err(|e| bad(format!("attestationObject: {e}")))?;
    if !rest.is_empty() {
        return Err(bad("attestationObject has bytes after it"));
    }
    let data = auth_data(obj.field("authData").and_then(Cbor::bytes).ok_or_else(|| bad("attestationObject has no authData"))?).map_err(|e| bad(format!("authData: {e}")))?;
    if data.rp_id_hash != sha256(p.rp_id.as_bytes()) {
        return Err(refused(format!("made for another domain than {}", p.rp_id)));
    }
    if data.flags & UP == 0 || data.flags & UV == 0 {
        return Err(refused("the authenticator didn't verify its person (user verification is required)"));
    }
    let (made_id, key) = data.made.ok_or_else(|| bad("authData has no credential"))?;
    if made_id != id {
        return Err(bad("the credential's id isn't the one sent"));
    }
    let (alg, _) = cose(&key).map_err(bad)?;
    let transports: Vec<String> = body.get("transports").and_then(Value::as_array).into_iter().flatten().filter_map(Value::as_str).take(8).map(str::to_owned).collect();
    let (kid, now) = (b64(&id), crate::util::now_unix());
    let (w, n, rp, backed) = (who.clone(), name.clone(), p.rp_id.clone(), data.flags & BS != 0);
    let (k2, t) = (kid.clone(), serde_json::to_string(&transports).unwrap_or_default());
    let kept = hub
        .store
        .call(move |c| {
            let n_has: i64 = c.query_row("SELECT COUNT(*) FROM passkeys WHERE principal = ?1", [&w], |r| r.get(0))?;
            if n_has >= PER_PERSON {
                return Ok(Err(format!("{PER_PERSON} passkeys is the most one keeps: remove one first")));
            }
            if c.query_row("SELECT 1 FROM passkeys WHERE id = ?1", [&k2], |_| Ok(())).optional()?.is_some() {
                return Ok(Err("that passkey is kept here already".to_owned()));
            }
            c.execute(
                "INSERT INTO passkeys (id, principal, name, rp_id, alg, public_key, sign_count, backed_up, transports, created_at) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
                params![k2, w, n, rp, alg, key, data.sign_count, backed, t, now],
            )?;
            Ok(Ok(()))
        })
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, format!("{e:#}")))?;
    kept.map_err(|e| (StatusCode::CONFLICT, e))?;
    hub.store.audit(caller.name.clone(), "passkey.add".into(), format!("{name} for {}", p.rp_id)).await;
    Ok(json!({ "id": kid, "name": name, "rp_id": p.rp_id, "created_at": now, "last_used_at": null, "backed_up": backed }))
}

/// `GET /api/me/passkeys`: the caller's passkeys, newest first.
pub async fn list(hub: &Hub, caller: &Caller) -> Result<Value, Fail> {
    let who = principal(caller)?;
    let rows: Vec<Value> = hub
        .store
        .call(move |c| {
            let mut st = c.prepare("SELECT id, name, rp_id, created_at, last_used_at, backed_up FROM passkeys WHERE principal = ?1 ORDER BY created_at DESC")?;
            let rows = st
                .query_map([&who], |r| {
                    Ok(json!({ "id": r.get::<_, String>(0)?, "name": r.get::<_, String>(1)?, "rp_id": r.get::<_, String>(2)?, "created_at": r.get::<_, i64>(3)?, "last_used_at": r.get::<_, Option<i64>>(4)?, "backed_up": r.get::<_, bool>(5)? }))
                })?
                .collect::<Result<Vec<_>, _>>()?;
            Ok(rows)
        })
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, format!("{e:#}")))?;
    Ok(json!({ "passkeys": rows }))
}

/// `DELETE /api/me/passkeys/:id`: one of the caller's own, gone.
pub async fn remove(hub: &Hub, caller: &Caller, id: &str) -> Result<(), Fail> {
    let who = principal(caller)?;
    let (w, i) = (who, id.to_owned());
    let name: Option<String> = hub
        .store
        .call(move |c| {
            let name: Option<String> = c.query_row("SELECT name FROM passkeys WHERE id = ?1 AND principal = ?2", params![i, w], |r| r.get(0)).optional()?;
            c.execute("DELETE FROM passkeys WHERE id = ?1 AND principal = ?2", params![i, w])?;
            Ok(name)
        })
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, format!("{e:#}")))?;
    let name = name.ok_or((StatusCode::NOT_FOUND, "no such passkey of yours".to_owned()))?;
    hub.store.audit(caller.name.clone(), "passkey.remove".into(), name).await;
    Ok(())
}

// ------------------------------------------------------------------ signing in

/// `POST /api/auth/passkey/options` (anyone): a challenge to sign in with a passkey of this
/// domain, which the person picks.
pub fn sign_in_options(hub: &Hub, asked_origin: Option<&str>) -> Result<Value, Fail> {
    let (origin, rp_id) = origin_of(hub, asked_origin)?;
    let c = challenge(Purpose::SignIn, &origin, &rp_id)?;
    Ok(json!({ "challenge": c, "rpId": rp_id, "timeout": CHALLENGE_TTL.as_millis(), "userVerification": "required", "allowCredentials": [] }))
}

/// Who a passkey signed in.
pub enum Who {
    Owner,
    /// A person: their id, name and session epoch.
    User { id: String, name: String, epoch: i64 },
}

/// `POST /api/auth/passkey` `{id, clientDataJSON, authenticatorData, signature, userHandle}`: the
/// passkey's signature of the challenge, checked; whose it is.
pub async fn sign_in(hub: &Hub, body: &Value) -> Result<Who, Fail> {
    let (id, client, data_raw, sig) = (field(body, "id")?, field(body, "clientDataJSON")?, field(body, "authenticatorData")?, field(body, "signature")?);
    let p = client_data(&client, "webauthn.get")?;
    if !matches!(p.purpose, Purpose::SignIn) {
        return Err(refused("that challenge wasn't for signing in"));
    }
    let kid = b64(&id);
    let k = kid.clone();
    let row: Option<(String, String, String, Vec<u8>, i64)> = hub
        .store
        .call(move |c| Ok(c.query_row("SELECT principal, name, rp_id, public_key, sign_count FROM passkeys WHERE id = ?1", [&k], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?))).optional()?))
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, format!("{e:#}")))?;
    let (who, name, rp_id, key, count) = row.ok_or_else(|| refused("that passkey isn't one this server knows: sign in another way, then make one in Settings"))?;
    if rp_id != p.rp_id {
        return Err(refused(format!("that passkey is {rp_id}'s, not {}'s", p.rp_id)));
    }
    let data = auth_data(&data_raw).map_err(|e| bad(format!("authenticatorData: {e}")))?;
    if data.rp_id_hash != sha256(rp_id.as_bytes()) {
        return Err(refused("signed for another domain"));
    }
    if data.flags & UP == 0 || data.flags & UV == 0 {
        return Err(refused("the authenticator didn't verify its person (user verification is required)"));
    }
    let (_, key) = cose(&key).map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e))?;
    if !verify(&key, &[data_raw.as_slice(), &sha256(&client)].concat(), &sig) {
        return Err(refused("the passkey's signature doesn't verify"));
    }
    // A counter that went backwards: two authenticators with one key.
    let now_count = i64::from(data.sign_count);
    if (count > 0 || now_count > 0) && now_count <= count {
        return Err(refused(format!("{name}'s counter went backwards: it may have been copied, so it's refused (remove it in Settings and make another)")));
    }
    let handle = body.get("userHandle").and_then(Value::as_str).filter(|h| !h.is_empty()).and_then(unb64);
    let user = who.strip_prefix("user:").map(str::to_owned);
    if let Some(h) = &handle
        && h.as_slice() != user.as_deref().unwrap_or("owner").as_bytes()
    {
        return Err(refused("the passkey's person isn't the one it was made for"));
    }
    let (k, now, backed) = (kid.clone(), crate::util::now_unix(), data.flags & BS != 0);
    let who_now = hub
        .store
        .call(move |c| {
            c.execute("UPDATE passkeys SET sign_count = ?2, last_used_at = ?3, backed_up = ?4 WHERE id = ?1", params![k, now_count, now, backed])?;
            match user {
                None => Ok(Some(Who::Owner)),
                Some(id) => {
                    let row: Option<(String, i64)> = c.query_row("SELECT name, session_epoch FROM users WHERE id = ?1 AND disabled_at IS NULL", [&id], |r| Ok((r.get(0)?, r.get(1)?))).optional()?;
                    Ok(row.map(|(name, epoch)| Who::User { id, name, epoch }))
                }
            }
        })
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, format!("{e:#}")))?;
    let who_now = who_now.ok_or_else(|| refused("this kiln user is disabled"))?;
    let actor = match &who_now {
        Who::Owner => "admin".to_owned(),
        Who::User { name, .. } => format!("user:{name}"),
    };
    hub.store.audit(actor, "login".into(), format!("passkey {name}")).await;
    Ok(who_now)
}

/// Whether anyone keeps a passkey here (the sign-in page offers it then).
pub async fn any(hub: &Hub) -> bool {
    hub.store.call(|c| Ok(c.query_row("SELECT EXISTS(SELECT 1 FROM passkeys)", [], |r| r.get::<_, bool>(0))?)).await.unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// W10: CBOR as WebAuthn writes it, and COSE keys of each kind kiln verifies.
    #[test]
    fn w10_passkeys_cbor_and_cose() -> Result<(), String> {
        // {1: 2, 3: -7, -1: 1, -2: h'00…', -3: h'01…'}
        let mut k = vec![0xa5, 0x01, 0x02, 0x03, 0x26, 0x20, 0x01, 0x21, 0x58, 0x20];
        k.extend([0u8; 32]);
        k.extend([0x22, 0x58, 0x20]);
        k.extend([1u8; 32]);
        let (alg, key) = cose(&k)?;
        assert_eq!(alg, ES256);
        assert!(matches!(key, Key::Es256(ref p) if p.len() == 65 && p.first() == Some(&4)));
        assert!(cose(k.get(..k.len() - 1).unwrap_or_default()).is_err(), "a key cut short");
        // {"fmt": "none", "attStmt": {}, "authData": h'0102'}
        let att = [0xa3, 0x63, b'f', b'm', b't', 0x64, b'n', b'o', b'n', b'e', 0x67, b'a', b't', b't', b'S', b't', b'm', b't', 0xa0, 0x68, b'a', b'u', b't', b'h', b'D', b'a', b't', b'a', 0x42, 1, 2];
        let (v, rest) = cbor(&att, 0)?;
        assert!(rest.is_empty());
        assert_eq!(v.field("fmt"), Some(&Cbor::Text("none".into())));
        assert_eq!(v.field("authData").and_then(Cbor::bytes), Some(&[1u8, 2][..]));
        assert!(cbor(&[0x5f], 0).is_err(), "indefinite lengths");
        assert!(cbor(&[0x9b, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff], 0).is_err(), "a length past its end");
        assert_eq!(origin("https://Kiln.Example.com:443/x"), "https://kiln.example.com");
        assert_eq!(origin("http://localhost:7070"), "http://localhost:7070");
        Ok(())
    }
}
