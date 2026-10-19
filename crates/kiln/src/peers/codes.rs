//! What peering is authorized with, and what a peered cluster's people carry (docs/peers.md).
//!
//! * **The key.** Each cluster has one Ed25519 key for peering, made once by its server and kept
//!   under its data directory. Its id is the first 8 bytes of the SHA-256 of the public key, in hex.
//! * **The challenge** (`kiln_pc1_…`) is what the cluster that wants to open another one makes: its
//!   name, its public key, a nonce and until when it may be answered. An admin carries it to the
//!   other cluster by hand.
//! * **The response** (`kiln_pr1_…`) is what that cluster's admin makes from it, authorizing it:
//!   their own name and public key, the role they gave, the challenge's nonce, and a signature
//!   over all of it and the challenger's key. Carried back by hand, it is checked against the
//!   challenge it answers, so it can't be replayed for another, and proves the key it carries.
//!   Nothing secret crosses either way, and the two clusters never need to reach each other.
//! * **A grant** (`kiln_peer_…`) is what a person carries into a peered cluster: who they are at
//!   home, the most they may do there (a role), for which cluster, until when, signed with the home
//!   cluster's key. The peered cluster checks it with the key it got in the challenge, by itself.

use std::path::Path;

use aws_lc_rs::rand::{SecureRandom as _, SystemRandom};
use aws_lc_rs::signature::{ED25519, Ed25519KeyPair, KeyPair as _, UnparsedPublicKey};
use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD as B64;
use serde::{Deserialize, Serialize};

pub const CHALLENGE_PREFIX: &str = "kiln_pc1_";
pub const RESPONSE_PREFIX: &str = "kiln_pr1_";
pub const GRANT_PREFIX: &str = "kiln_peer_";

const NONCE_LEN: usize = 16;
const KEY_LEN: usize = 32;
/// The longest name a code may carry.
const NAME_MAX: usize = 100;

/// The roles a peer may be given, lowest first (`owner` is never a peer's).
pub const ROLES: &[&str] = &["viewer", "developer", "maintainer", "admin"];

/// The lower of two roles; an unknown one counts as none, and `owner` as `admin`.
pub fn lower<'a>(a: &'a str, b: &'a str) -> Option<&'static str> {
    let rank = |r: &str| ROLES.iter().position(|x| *x == if r == "owner" { "admin" } else { r });
    ROLES.get(rank(a)?.min(rank(b)?)).copied()
}

/// This cluster's peering key.
pub struct Key {
    pair: Ed25519KeyPair,
    public: Vec<u8>,
}

impl Key {
    /// Loads the key under `dir`, or makes it (once, readable by this user alone).
    pub fn open(dir: &Path) -> anyhow::Result<Key> {
        let path = dir.join("peering.p8");
        let pkcs8 = match std::fs::read(&path) {
            Ok(b) => b,
            Err(_) => {
                std::fs::create_dir_all(dir)?;
                let doc = Ed25519KeyPair::generate_pkcs8(&SystemRandom::new()).map_err(|_| anyhow::anyhow!("no peering key could be made"))?;
                crate::runner::auth::write_private(&path, doc.as_ref())?;
                doc.as_ref().to_vec()
            }
        };
        Self::from_pkcs8(&pkcs8).map_err(|e| anyhow::anyhow!("{}: {e:#}", path.display()))
    }

    fn from_pkcs8(pkcs8: &[u8]) -> anyhow::Result<Key> {
        let pair = Ed25519KeyPair::from_pkcs8(pkcs8).map_err(|_| anyhow::anyhow!("not an Ed25519 key"))?;
        let public = pair.public_key().as_ref().to_vec();
        Ok(Key { pair, public })
    }

    /// A key of its own, kept nowhere (tests).
    pub fn generate() -> anyhow::Result<Key> {
        let doc = Ed25519KeyPair::generate_pkcs8(&SystemRandom::new()).map_err(|_| anyhow::anyhow!("no key could be made"))?;
        Self::from_pkcs8(doc.as_ref())
    }

    pub fn public(&self) -> &[u8] {
        &self.public
    }

    pub fn id(&self) -> String {
        key_id(&self.public)
    }

    pub fn sign(&self, msg: &[u8]) -> Vec<u8> {
        self.pair.sign(msg).as_ref().to_vec()
    }
}

/// A public key's id: the first 8 bytes of its SHA-256, in hex.
pub fn key_id(public: &[u8]) -> String {
    crate::util::sha256_hex(public).chars().take(16).collect()
}

/// A public key as people compare it: the first 16 bytes of its SHA-256, in groups of four.
pub fn fingerprint(public: &[u8]) -> String {
    let hex: Vec<char> = crate::util::sha256_hex(public).chars().take(32).collect();
    hex.chunks(4).map(|c| c.iter().collect::<String>()).collect::<Vec<_>>().join(" ")
}

/// Whether `sig` is `public`'s Ed25519 signature of `msg`.
pub fn verify(public: &[u8], msg: &[u8], sig: &[u8]) -> bool {
    UnparsedPublicKey::new(&ED25519, public).verify(msg, sig).is_ok()
}

pub fn encode_key(public: &[u8]) -> String {
    B64.encode(public)
}

/// A public key from its base64url text, when it is one.
pub fn decode_key(text: &str) -> anyhow::Result<Vec<u8>> {
    let k = B64.decode(text.trim()).map_err(|_| anyhow::anyhow!("its key isn't base64url"))?;
    anyhow::ensure!(k.len() == KEY_LEN, "its key isn't an Ed25519 public key");
    Ok(k)
}

/// A code's JSON, after its prefix (whitespace and line breaks a copy picked up are ignored).
fn body<T: serde::de::DeserializeOwned>(code: &str, prefix: &str, what: &str) -> anyhow::Result<T> {
    let code: String = code.chars().filter(|c| !c.is_whitespace()).collect();
    let rest = code.strip_prefix(prefix).ok_or_else(|| anyhow::anyhow!("that isn't a {what} (it starts with {prefix})"))?;
    let json = B64.decode(rest).map_err(|_| anyhow::anyhow!("the {what} was cut short or changed"))?;
    serde_json::from_slice(&json).map_err(|_| anyhow::anyhow!("the {what} was cut short or changed"))
}

fn encode<T: Serialize>(prefix: &str, v: &T) -> anyhow::Result<String> {
    Ok(format!("{prefix}{}", B64.encode(serde_json::to_vec(v)?)))
}

fn check_name(n: &str) -> anyhow::Result<()> {
    anyhow::ensure!(!n.trim().is_empty() && n.chars().count() <= NAME_MAX && !n.chars().any(char::is_control), "its name is empty, too long or unprintable");
    Ok(())
}

/// What a cluster that wants to open another one asks it with.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Challenge {
    /// The asking cluster's name.
    pub n: String,
    /// Its public key (base64url).
    pub k: String,
    /// A nonce, so a response answers this challenge alone.
    pub c: String,
    /// Until when it may be answered (unix seconds).
    pub x: i64,
    /// Its address as its admin saw it: shown to the other side, used by nothing.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub u: String,
}

impl Challenge {
    /// A new challenge from `key`, answerable for `ttl` seconds.
    pub fn new(name: &str, key: &Key, url: &str, now: i64, ttl: i64) -> anyhow::Result<Challenge> {
        let mut nonce = [0u8; NONCE_LEN];
        SystemRandom::new().fill(&mut nonce).map_err(|_| anyhow::anyhow!("no randomness"))?;
        Ok(Challenge { n: name.to_owned(), k: encode_key(key.public()), c: B64.encode(nonce), x: now + ttl, u: url.to_owned() })
    }

    pub fn encode(&self) -> anyhow::Result<String> {
        encode(CHALLENGE_PREFIX, self)
    }

    /// The challenge `code` carries, when it is a well-formed one.
    pub fn decode(code: &str) -> anyhow::Result<Challenge> {
        let c: Challenge = body(code, CHALLENGE_PREFIX, "challenge")?;
        check_name(&c.n)?;
        decode_key(&c.k)?;
        anyhow::ensure!(B64.decode(&c.c).map(|n| n.len()) == Ok(NONCE_LEN), "the challenge was cut short or changed");
        Ok(c)
    }

    pub fn key(&self) -> anyhow::Result<Vec<u8>> {
        decode_key(&self.k)
    }
}

/// The signed answer to a challenge: the cluster that was asked authorized the one that asked.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Response {
    /// The answering cluster's name.
    pub n: String,
    /// Its public key (base64url).
    pub k: String,
    /// The challenge's nonce.
    pub c: String,
    /// The role it gave the asking cluster's people.
    pub r: String,
    /// Its CA (DER, base64url): what its HTTPS certificates chain to, trusted for its address.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub a: String,
    /// Its signature (base64url) over [`response_message`].
    pub s: String,
}

/// What a response signs: the challenger's key and nonce, the responder's key, the role, and
/// the responder's CA.
fn response_message(challenger: &[u8], nonce: &str, responder: &[u8], role: &str, ca: &str) -> Vec<u8> {
    let mut m = b"kiln peer response\0".to_vec();
    m.extend_from_slice(challenger);
    m.extend_from_slice(nonce.as_bytes());
    m.push(0);
    m.extend_from_slice(responder);
    m.extend_from_slice(role.as_bytes());
    m.push(0);
    m.extend_from_slice(ca.as_bytes());
    m
}

impl Response {
    /// `key`'s answer to `challenge`, giving its cluster's people `role`; `ca_der`: the answering
    /// cluster's CA.
    pub fn answer(challenge: &Challenge, name: &str, key: &Key, role: &str, ca_der: &[u8]) -> anyhow::Result<Response> {
        let a = B64.encode(ca_der);
        let sig = key.sign(&response_message(&challenge.key()?, &challenge.c, key.public(), role, &a));
        Ok(Response { n: name.to_owned(), k: encode_key(key.public()), c: challenge.c.clone(), r: role.to_owned(), a, s: B64.encode(sig) })
    }

    pub fn encode(&self) -> anyhow::Result<String> {
        encode(RESPONSE_PREFIX, self)
    }

    pub fn decode(code: &str) -> anyhow::Result<Response> {
        let r: Response = body(code, RESPONSE_PREFIX, "response")?;
        check_name(&r.n)?;
        decode_key(&r.k)?;
        Ok(r)
    }

    /// The answering cluster's public key, when this answers the challenge made with
    /// `challenger` and `nonce`, signed by the key it carries.
    pub fn verify(&self, challenger: &[u8], nonce: &str) -> anyhow::Result<Vec<u8>> {
        anyhow::ensure!(crate::util::ct_eq(self.c.as_bytes(), nonce.as_bytes()), "that response answers another challenge (or an older one)");
        let theirs = decode_key(&self.k)?;
        let sig = B64.decode(&self.s).map_err(|_| anyhow::anyhow!("the response was cut short or changed"))?;
        anyhow::ensure!(verify(&theirs, &response_message(challenger, nonce, &theirs, &self.r, &self.a), &sig), "the response was changed, or wasn't made from this challenge");
        Ok(theirs)
    }
}

/// What a person carries into a peered cluster.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Grant {
    /// The issuing (home) cluster's key id.
    pub i: String,
    /// The peered cluster's key id: the only one it's good at.
    pub a: String,
    /// The person, as home knows them: an id, and a name to show.
    pub s: String,
    pub n: String,
    /// The most they may do there: a role.
    pub r: String,
    /// Until when (unix seconds).
    pub e: i64,
}

fn grant_message(body: &str) -> Vec<u8> {
    [&b"kiln peer grant\0"[..], body.as_bytes()].concat()
}

impl Grant {
    /// The grant, signed with `key`: `kiln_peer_<claims>.<signature>`.
    pub fn sign(&self, key: &Key) -> anyhow::Result<String> {
        let body = B64.encode(serde_json::to_vec(self)?);
        let sig = key.sign(&grant_message(&body));
        Ok(format!("{GRANT_PREFIX}{body}.{}", B64.encode(sig)))
    }

    /// Its claims, unchecked: which key to check it with.
    pub fn peek(token: &str) -> Option<Grant> {
        let (body, _) = token.strip_prefix(GRANT_PREFIX)?.split_once('.')?;
        serde_json::from_slice(&B64.decode(body).ok()?).ok()
    }

    /// The grant `token` carries, when `public` signed it, it is for `audience`, and it hasn't
    /// expired by `now`.
    pub fn verify(token: &str, public: &[u8], audience: &str, now: i64) -> Option<Grant> {
        let (body, sig) = token.strip_prefix(GRANT_PREFIX)?.split_once('.')?;
        if !verify(public, &grant_message(body), &B64.decode(sig).ok()?) {
            return None;
        }
        let g: Grant = serde_json::from_slice(&B64.decode(body).ok()?).ok()?;
        (g.a == audience && g.e > now && g.i == key_id(public)).then_some(g)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_response_answers_its_challenge_alone() -> anyhow::Result<()> {
        let (home, away) = (Key::generate()?, Key::generate()?);
        let ch = Challenge::new("home", &home, "https://kiln.home", 1_000, 1_800)?;
        let code = ch.encode()?;
        // Wrapped and indented by a mail client on the way: still the same code.
        let mangled = format!("  {}\n  {}", code.get(..20).unwrap_or_default(), code.get(20..).unwrap_or_default());
        let got = Challenge::decode(&mangled)?;
        assert_eq!(got, ch);
        let resp = Response::decode(&Response::answer(&got, "away", &away, "developer", b"CA DER")?.encode()?)?;
        assert_eq!(resp.verify(home.public(), &ch.c)?, away.public());
        // Another challenge's nonce, a changed role, someone else's key: refused.
        let other = Challenge::new("home", &home, "", 1_000, 1_800)?;
        assert!(resp.verify(home.public(), &other.c).is_err());
        assert!(Response { r: "admin".into(), ..resp.clone() }.verify(home.public(), &ch.c).is_err());
        // Nor can the CA it vouches for be swapped.
        assert!(Response { a: B64.encode(b"another CA"), ..resp.clone() }.verify(home.public(), &ch.c).is_err());
        let thief = Key::generate()?;
        assert!(Response { k: encode_key(thief.public()), ..resp.clone() }.verify(home.public(), &ch.c).is_err());
        // Answered for another challenger: refused.
        assert!(resp.verify(thief.public(), &ch.c).is_err());
        Ok(())
    }

    #[test]
    fn codes_that_arent_say_so() {
        let e = |r: anyhow::Result<Challenge>| r.err().map(|e| e.to_string()).unwrap_or_default();
        assert!(e(Challenge::decode("kiln_pr1_abc")).contains("isn't a challenge"));
        assert!(e(Challenge::decode("kiln_pc1_!!!")).contains("cut short or changed"));
        let short = B64.encode(br#"{"n":"x","k":"AAAA","c":"AAAA","x":1}"#);
        assert!(e(Challenge::decode(&format!("kiln_pc1_{short}"))).contains("key"));
    }

    #[test]
    fn a_grant_is_good_for_its_cluster_until_it_expires() -> anyhow::Result<()> {
        let (home, away) = (Key::generate()?, Key::generate()?);
        let g = Grant { i: home.id(), a: away.id(), s: "u1".into(), n: "ada".into(), r: "developer".into(), e: 2_000 };
        let token = g.sign(&home)?;
        assert_eq!(Grant::peek(&token).map(|p| p.i), Some(home.id()));
        assert_eq!(Grant::verify(&token, home.public(), &away.id(), 1_999), Some(g.clone()));
        assert_eq!(Grant::verify(&token, home.public(), &away.id(), 2_000), None, "expired");
        assert_eq!(Grant::verify(&token, home.public(), &home.id(), 1_000), None, "for another cluster");
        assert_eq!(Grant::verify(&token, away.public(), &away.id(), 1_000), None, "someone else's key");
        // A changed claim breaks the signature.
        let (body, sig) = token.strip_prefix(GRANT_PREFIX).and_then(|t| t.split_once('.')).unwrap_or_default();
        let forged = B64.encode(serde_json::to_vec(&Grant { r: "admin".into(), ..g })?);
        assert_ne!(forged, body);
        assert_eq!(Grant::verify(&format!("{GRANT_PREFIX}{forged}.{sig}"), home.public(), &away.id(), 1_000), None);
        Ok(())
    }

    #[test]
    fn roles_meet_at_the_lower() {
        assert_eq!(lower("admin", "developer"), Some("developer"));
        assert_eq!(lower("owner", "maintainer"), Some("maintainer"));
        assert_eq!(lower("owner", "owner"), Some("admin"));
        assert_eq!(lower("viewer", "nobody"), None);
    }

    #[test]
    fn a_key_is_made_once_and_kept() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let (a, b) = (Key::open(dir.path())?, Key::open(dir.path())?);
        assert_eq!(a.public(), b.public());
        assert_eq!(fingerprint(a.public()).split(' ').count(), 8);
        Ok(())
    }
}
