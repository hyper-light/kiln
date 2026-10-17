//! The Tailscale API (`https://api.tailscale.com/api/v2`), as far as kiln needs it: a tailnet's
//! DNS settings.
//!
//! * **Credentials.** An OAuth client (`tskey-client-…`, made under *Trust credentials* in the
//!   admin console, with the `dns` scope) never expires; kiln trades it for an access token (an
//!   hour each) when it needs one. An API access token (`tskey-api-…`) works too, but acts as the
//!   person who made it and expires after 1 to 90 days.
//! * **Split DNS.** `GET`/`PATCH /tailnet/-/dns/split-dns` change only the domains named. The
//!   newer `/tailnet/-/dns/configuration` (the whole DNS configuration, replaced as one) is what
//!   can mark a route "use with exit node": without that, a device using an exit node asks the
//!   exit node instead, for every domain.
//!
//! The tailnet is `-`: the one the credential belongs to.

use std::collections::BTreeMap;
use std::fmt;
use std::time::{Duration, Instant};

use parking_lot::Mutex;
use serde::{Deserialize, Serialize};

use crate::http::client::Method;

pub const BASE: &str = "https://api.tailscale.com/api/v2";
const BODY_LIMIT: usize = 1 << 20;
const TIMEOUT: Duration = Duration::from_secs(20);
/// An access token is renewed this long before Tailscale says it expires.
const TOKEN_MARGIN: Duration = Duration::from_secs(120);

/// What kiln uses to call the API.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Credential {
    /// An OAuth client with the `dns` scope.
    Oauth { client_id: String, secret: String },
    /// An API access token.
    Token { token: String },
}

impl fmt::Debug for Credential {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Oauth { client_id, .. } => f.debug_struct("Oauth").field("client_id", client_id).field("secret", &"<redacted>").finish(),
            Self::Token { .. } => f.debug_struct("Token").field("token", &"<redacted>").finish(),
        }
    }
}

/// `tskey-client-<id>-<secret>`: the client's ID.
fn client_id_of(secret: &str) -> Option<String> {
    let rest = secret.strip_prefix("tskey-client-")?;
    let (id, tail) = rest.split_once('-')?;
    (!id.is_empty() && !tail.is_empty() && id.bytes().all(|b| b.is_ascii_alphanumeric())).then(|| id.to_owned())
}

impl Credential {
    /// What a person pasted: an OAuth client's secret (its ID, when not given, is the one the
    /// secret carries) or an API access token.
    pub fn parse(secret: &str, client_id: &str) -> Result<Self, String> {
        let (secret, client_id) = (secret.trim(), client_id.trim());
        if secret.is_empty() {
            return Err("Paste an OAuth client secret (tskey-client-…) or an API access token (tskey-api-…).".to_owned());
        }
        let printable = |s: &str| s.len() <= 512 && s.bytes().all(|b| b.is_ascii_graphic());
        if !printable(secret) || !printable(client_id) {
            return Err("That is not a Tailscale key: one line of letters, digits and dashes.".to_owned());
        }
        if secret.starts_with("tskey-api-") {
            return Ok(Self::Token { token: secret.to_owned() });
        }
        let id = if client_id.is_empty() { client_id_of(secret) } else { Some(client_id.to_owned()) };
        match id {
            Some(client_id) if secret.starts_with("tskey-client-") || !client_id.is_empty() => Ok(Self::Oauth { client_id, secret: secret.to_owned() }),
            _ => Err("Expected an OAuth client secret (tskey-client-…) with its client ID, or an API access token (tskey-api-…).".to_owned()),
        }
    }

    /// `oauth` or `token`.
    pub fn kind(&self) -> &'static str {
        match self {
            Self::Oauth { .. } => "oauth",
            Self::Token { .. } => "token",
        }
    }

    /// What may be shown of it: the OAuth client's ID, or the token's first characters.
    pub fn public(&self) -> String {
        match self {
            Self::Oauth { client_id, .. } => client_id.clone(),
            Self::Token { token } => format!("{}…", token.chars().take(16).collect::<String>()),
        }
    }
}

/// Why a call failed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Error {
    /// 401: the key is not (or no longer) valid.
    Unauthorized(String),
    /// 403: valid, but not allowed to do this.
    Forbidden(String),
    /// 404: no such tailnet, or no such endpoint (an older API).
    NotFound(String),
    /// The network, a server error, an answer kiln cannot read.
    Other(String),
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Unauthorized(m) => write!(f, "Tailscale refused the key ({m}): it was revoked, expired, or mistyped"),
            Self::Forbidden(m) => write!(f, "Tailscale says the key may not change DNS ({m}): an OAuth client needs the dns scope"),
            Self::NotFound(m) => write!(f, "Tailscale has no such thing ({m})"),
            Self::Other(m) => f.write_str(m),
        }
    }
}

impl std::error::Error for Error {}

/// `application/x-www-form-urlencoded` for one value.
fn form_value(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'.' | b'_' | b'~') {
            out.push(char::from(b));
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

/// One tailnet's API.
pub struct Api {
    base: String,
    credential: Credential,
    http: crate::http::client::Client,
    token: Mutex<Option<(String, Instant)>>,
}

#[derive(Deserialize)]
struct Token {
    access_token: String,
    #[serde(default)]
    expires_in: u64,
}

impl Api {
    pub fn new(credential: Credential) -> anyhow::Result<Self> {
        Self::with_base(credential, BASE)
    }

    /// Against another server (tests).
    pub fn with_base(credential: Credential, base: &str) -> anyhow::Result<Self> {
        Ok(Self { base: base.trim_end_matches('/').to_owned(), credential, http: crate::http::client::Client::public()?, token: Mutex::new(None) })
    }

    pub fn credential(&self) -> &Credential {
        &self.credential
    }

    /// The bearer token for the next call: the API access token itself, or an OAuth access
    /// token (fetched again shortly before the last one expires).
    async fn bearer(&self) -> Result<String, Error> {
        let (client_id, secret) = match &self.credential {
            Credential::Token { token } => return Ok(token.clone()),
            Credential::Oauth { client_id, secret } => (client_id, secret),
        };
        if let Some((t, until)) = self.token.lock().clone()
            && Instant::now() < until
        {
            return Ok(t);
        }
        let body = format!("client_id={}&client_secret={}&grant_type=client_credentials", form_value(client_id), form_value(secret));
        let resp = self
            .http
            .post(&format!("{}/oauth/token", self.base))
            .header("Content-Type", "application/x-www-form-urlencoded")
            .header("Accept", "application/json")
            .body(body)
            .timeout(TIMEOUT)
            .send()
            .await
            .map_err(|e| Error::Other(format!("Tailscale's API cannot be reached: {e:#}")))?;
        let status = resp.status();
        let bytes = resp.bytes(BODY_LIMIT).await.map_err(|e| Error::Other(format!("{e:#}")))?;
        if !(200..300).contains(&status) {
            // A wrong ID or secret is "unauthorized" whatever the status says.
            let why = message(&bytes).unwrap_or_else(|| format!("HTTP {status}"));
            return Err(if status >= 500 { Error::Other(format!("Tailscale's token endpoint: {why}")) } else { Error::Unauthorized(why) });
        }
        let t: Token = serde_json::from_slice(&bytes).map_err(|e| Error::Other(format!("Tailscale's token answer: {e}")))?;
        let life = Duration::from_secs(t.expires_in.max(60));
        let until = Instant::now() + life.saturating_sub(TOKEN_MARGIN).max(Duration::from_secs(30));
        *self.token.lock() = Some((t.access_token.clone(), until));
        Ok(t.access_token)
    }

    /// One call: JSON in, JSON out (`null` for an empty answer). An OAuth access token refused
    /// once is fetched again, once.
    async fn call(&self, method: Method, path: &str, body: Option<&serde_json::Value>) -> Result<serde_json::Value, Error> {
        let mut retried = false;
        loop {
            let token = self.bearer().await?;
            let mut req = self.http.request(method, &format!("{}{path}", self.base)).bearer(&token).header("Accept", "application/json").timeout(TIMEOUT);
            if let Some(b) = body {
                req = req.json(b);
            }
            let resp = req.send().await.map_err(|e| Error::Other(format!("Tailscale's API cannot be reached: {e:#}")))?;
            let status = resp.status();
            let bytes = resp.bytes(BODY_LIMIT).await.map_err(|e| Error::Other(format!("{e:#}")))?;
            if (200..300).contains(&status) {
                if bytes.iter().all(u8::is_ascii_whitespace) {
                    return Ok(serde_json::Value::Null);
                }
                return serde_json::from_slice(&bytes).map_err(|e| Error::Other(format!("Tailscale's answer to {path}: {e}")));
            }
            let why = message(&bytes).unwrap_or_else(|| format!("HTTP {status}"));
            match status {
                401 if !retried && matches!(self.credential, Credential::Oauth { .. }) => {
                    *self.token.lock() = None;
                    retried = true;
                }
                401 => return Err(Error::Unauthorized(why)),
                403 => return Err(Error::Forbidden(why)),
                404 => return Err(Error::NotFound(why)),
                _ => return Err(Error::Other(format!("Tailscale's API: {why} (HTTP {status})"))),
            }
        }
    }

    /// The tailnet's split DNS: domain → nameserver addresses.
    pub async fn split_dns(&self) -> Result<BTreeMap<String, Vec<String>>, Error> {
        Ok(split_dns_of(&self.call(Method::Get, "/tailnet/-/dns/split-dns", None).await?))
    }

    /// Change the split DNS of the domains named (`None` clears a domain); the others stay.
    pub async fn update_split_dns(&self, changes: &BTreeMap<String, Option<Vec<String>>>) -> Result<BTreeMap<String, Vec<String>>, Error> {
        let body = serde_json::to_value(changes).map_err(|e| Error::Other(e.to_string()))?;
        Ok(split_dns_of(&self.call(Method::Patch, "/tailnet/-/dns/split-dns", Some(&body)).await?))
    }

    /// The tailnet's whole DNS configuration (nameservers, split DNS, search paths, preferences).
    pub async fn configuration(&self) -> Result<serde_json::Value, Error> {
        let v = self.call(Method::Get, "/tailnet/-/dns/configuration", None).await?;
        if v.is_object() { Ok(v) } else { Err(Error::Other("Tailscale's DNS configuration is not an object".to_owned())) }
    }

    /// Replace the tailnet's whole DNS configuration (give back everything [`Self::configuration`]
    /// returned, changed where meant).
    pub async fn set_configuration(&self, config: &serde_json::Value) -> Result<serde_json::Value, Error> {
        self.call(Method::Post, "/tailnet/-/dns/configuration", Some(config)).await
    }
}

/// The API's `{"message": "…"}`.
fn message(body: &[u8]) -> Option<String> {
    let v: serde_json::Value = serde_json::from_slice(body).ok()?;
    let m = v.get("message")?.as_str()?.trim();
    (!m.is_empty()).then(|| m.chars().take(300).collect())
}

/// `{"example.com": ["1.2.3.4"], "gone.com": null}` as domain → addresses (null ones left out).
pub fn split_dns_of(v: &serde_json::Value) -> BTreeMap<String, Vec<String>> {
    let mut out = BTreeMap::new();
    if let Some(m) = v.as_object() {
        for (domain, list) in m {
            if let Some(a) = list.as_array() {
                out.insert(domain.clone(), a.iter().filter_map(|x| x.as_str().map(str::to_owned)).collect());
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn what_people_paste_is_understood() {
        let c = Credential::parse(" tskey-client-kAbC123CNTRL-SeCrEt456 ", "");
        assert_eq!(c, Ok(Credential::Oauth { client_id: "kAbC123CNTRL".into(), secret: "tskey-client-kAbC123CNTRL-SeCrEt456".into() }));
        assert_eq!(c.as_ref().map(Credential::public).as_deref(), Ok("kAbC123CNTRL"));
        // A client ID given wins over the one the secret carries.
        assert!(matches!(Credential::parse("tskey-client-kA-x", "kOther"), Ok(Credential::Oauth { client_id, .. }) if client_id == "kOther"));
        let t = Credential::parse("tskey-api-k123CNTRL-abcdefghijklmnop", "");
        assert!(matches!(&t, Ok(Credential::Token { .. })));
        assert_eq!(t.as_ref().map(Credential::kind), Ok("token"));
        assert_eq!(t.as_ref().map(Credential::public).as_deref(), Ok("tskey-api-k123CN…"));
        for bad in ["", "   ", "hello world", "tskey-client-", "tskey-auth-k1-abc", "tskey-client-k1-a\nb"] {
            assert!(Credential::parse(bad, "").is_err(), "{bad:?}");
        }
        // Logs never carry the secret.
        let shown = format!("{:?}", Credential::Oauth { client_id: "kA".into(), secret: "tskey-client-kA-TOPSECRET".into() });
        assert!(shown.contains("kA") && !shown.contains("TOPSECRET"), "{shown}");
        assert!(!format!("{:?}", Credential::Token { token: "tskey-api-TOPSECRET".into() }).contains("TOPSECRET"));
    }

    #[test]
    fn forms_and_answers_are_encoded_and_read() -> anyhow::Result<()> {
        assert_eq!(form_value("tskey-client-k1-a+b/c=d e"), "tskey-client-k1-a%2Bb%2Fc%3Dd%20e");
        let m = split_dns_of(&serde_json::json!({ "kiln.dev": ["100.64.0.1", 7], "gone.example": null, "x": "y" }));
        assert_eq!(m.get("kiln.dev").map(Vec::as_slice), Some(&["100.64.0.1".to_owned()][..]));
        assert_eq!(m.len(), 1);
        assert_eq!(message(br#"{"message":"  requires scope dns  "}"#).as_deref(), Some("requires scope dns"));
        assert_eq!(message(b"<html>"), None);
        assert!(Error::Forbidden("x".into()).to_string().contains("dns scope"));
        Ok(())
    }
}
