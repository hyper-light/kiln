//! The cache service's tokens (docs/ci.md W24-R08): each scoped to one repository, one trust
//! domain and read or write, and signed by this runner (HMAC-SHA256 under a key only it holds, in
//! `<work>/_cas/key`), so the service checks one without asking anyone.
//!
//! What a token reads follows W24-R21: its own scope, and for an untrusted or a person's token the
//! trusted entries too. Reads go down only: a trusted or protected token reads nothing else. A
//! token writes its own scope alone.

use std::path::Path;

use base64::Engine as _;
use serde::{Deserialize, Serialize};

use super::store::Scope;

const B64: base64::engine::GeneralPurpose = base64::engine::general_purpose::URL_SAFE_NO_PAD;
/// Every token starts so: a token of another kind (a leg's, an agent's) is never mistaken for one.
const PREFIX: &str = "kcas1";

/// What a token lets its holder do.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Claims {
    pub repo: String,
    pub domain: String,
    #[serde(default)]
    pub read: bool,
    #[serde(default)]
    pub write: bool,
    /// When it stops working, in seconds since the epoch.
    pub exp: i64,
    /// The grant it was minted under, whose writes go up to the server (`super::upstream`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub grant: Option<String>,
}

/// Whether a domain is a person's own (`person:<id>`).
pub fn is_person(domain: &str) -> bool {
    domain.strip_prefix("person:").is_some_and(|id| !id.is_empty())
}

/// Whether `domain` is one of W24-R21's.
pub fn known_domain(domain: &str) -> bool {
    matches!(domain, "trusted" | "protected" | "untrusted") || is_person(domain)
}

impl Claims {
    /// Its own scope.
    pub fn scope(&self) -> Scope {
        Scope::new(&self.repo, &self.domain)
    }

    /// The scopes it reads, its own first (W24-R21: untrusted and person scopes read trusted
    /// entries; trusted and protected read only their own). Empty when it doesn't read.
    pub fn reads(&self) -> Vec<Scope> {
        if !self.read {
            return Vec::new();
        }
        let mut s = vec![self.scope()];
        if self.domain == "untrusted" || is_person(&self.domain) {
            s.push(Scope::new(&self.repo, "trusted"));
        }
        s
    }

    /// The scope it writes, if it does.
    pub fn writes(&self) -> Option<Scope> {
        self.write.then(|| self.scope())
    }
}

/// The key tokens are signed with.
pub struct Keyring {
    key: Vec<u8>,
}

impl std::fmt::Debug for Keyring {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Keyring")
    }
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

impl Keyring {
    /// The key kept in `dir/key`, made the first time (owner-only): tokens outlive a restart.
    pub fn open(dir: &Path) -> anyhow::Result<Keyring> {
        let path = dir.join("key");
        if let Ok(key) = std::fs::read(&path)
            && key.len() >= 32
        {
            return Ok(Keyring { key });
        }
        let key: [u8; 32] = rand::random();
        crate::util::write_secret(&path, &key, 0o600)?;
        Ok(Keyring { key: key.to_vec() })
    }

    /// A key of its own, kept nowhere (tests).
    pub fn ephemeral() -> Keyring {
        let key: [u8; 32] = rand::random();
        Keyring { key: key.to_vec() }
    }

    /// A token for `claims`.
    pub fn mint(&self, claims: &Claims) -> String {
        let body = B64.encode(serde_json::to_vec(claims).unwrap_or_default());
        let signing = format!("{PREFIX}.{body}");
        let sig = B64.encode(hmac(&self.key, signing.as_bytes()));
        format!("{signing}.{sig}")
    }

    /// What `token` lets its holder do at `now`, or why it doesn't.
    pub fn verify(&self, token: &str, now: i64) -> Result<Claims, &'static str> {
        let mut parts = token.trim().split('.');
        let (Some(prefix), Some(body), Some(sig), None) = (parts.next(), parts.next(), parts.next(), parts.next()) else { return Err("not a token of this runner's cache") };
        if prefix != PREFIX {
            return Err("not a token of this runner's cache");
        }
        let want = hmac(&self.key, format!("{prefix}.{body}").as_bytes());
        let got = B64.decode(sig).map_err(|_| "a token whose signature doesn't read")?;
        if want.is_empty() || !crate::util::ct_eq(&want, &got) {
            return Err("a token this runner didn't sign");
        }
        let claims: Claims = B64.decode(body).ok().and_then(|b| serde_json::from_slice(&b).ok()).ok_or("a token whose claims don't read")?;
        if claims.exp <= now {
            return Err("an expired token");
        }
        Ok(claims)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn claims(repo: &str, domain: &str, read: bool, write: bool) -> Claims {
        Claims { repo: repo.into(), domain: domain.into(), read, write, exp: 2_000_000_000, grant: None }
    }

    /// W24-R08, R21: a token names one repository, one domain, and read or write; what it reads
    /// goes down only.
    #[test]
    fn w24_r08_tokens_scope_reads_and_writes() -> anyhow::Result<()> {
        let keys = Keyring::ephemeral();
        let t = keys.mint(&claims("acme/app", "person:ada", true, true));
        let c = keys.verify(&t, 1_900_000_000).map_err(anyhow::Error::msg)?;
        assert_eq!(c.reads(), vec![Scope::new("acme/app", "person:ada"), Scope::new("acme/app", "trusted")]);
        assert_eq!(c.writes(), Some(Scope::new("acme/app", "person:ada")));
        assert_eq!(claims("acme/app", "untrusted", true, false).reads(), vec![Scope::new("acme/app", "untrusted"), Scope::new("acme/app", "trusted")]);
        assert_eq!(claims("acme/app", "trusted", true, true).reads(), vec![Scope::new("acme/app", "trusted")], "trusted reads only its own");
        assert_eq!(claims("acme/app", "protected", true, false).reads(), vec![Scope::new("acme/app", "protected")]);
        assert!(claims("acme/app", "trusted", false, true).reads().is_empty() && claims("acme/app", "trusted", true, false).writes().is_none());
        // Expired, signed by another runner, edited, or not one of ours: refused.
        assert!(keys.verify(&t, 2_000_000_000).is_err());
        assert!(Keyring::ephemeral().verify(&t, 1).is_err());
        let mut parts: Vec<String> = t.split('.').map(str::to_owned).collect();
        if let Some(body) = parts.get_mut(1) {
            *body = B64.encode(serde_json::to_vec(&claims("acme/other", "trusted", true, true))?);
        }
        assert!(keys.verify(&parts.join("."), 1).is_err(), "claims changed after signing");
        assert!(keys.verify("eyJhbGciOiJIUzI1NiJ9.e30.sig", 1).is_err());
        // The key is kept: a token outlives a restart.
        let dir = tempfile::tempdir()?;
        let a = Keyring::open(dir.path())?;
        let b = Keyring::open(dir.path())?;
        assert!(b.verify(&a.mint(&claims("acme/app", "trusted", true, true)), 1).is_ok());
        assert!(known_domain("person:ada") && !known_domain("person:") && !known_domain("everyone"));
        Ok(())
    }
}
