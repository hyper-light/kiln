//! Linked kiln servers (docs/deploy-api.md §11). An environment's `after` may name an environment of
//! another kiln (`staging@eu`): that server is an integration of kind `kiln` here (its URL, and a
//! token of an account there that reads its releases, `runs.read`). Asked what runs of an app in
//! one of its environments, it answers with a statement it signs with its OIDC issuer (docs/ci.md
//! W10), for this server alone and for five minutes. The statement is checked here against the
//! keys its issuer publishes before a gate takes it, and kept with the gate's result: anyone can
//! check it again.

use base64::Engine as _;
use serde_json::{Map, Value, json};

use super::Deploy;
use super::store;
use crate::http::client::{Client, Method, Roots};
use crate::hub::Hub;

/// The integration kind a linked server is.
pub const KIND: &str = "kiln";

/// How long a statement holds.
const LIFETIME_SECS: i64 = 300;
/// Clocks a little apart still agree.
const LEEWAY_SECS: i64 = 60;

const B64: base64::engine::GeneralPurpose = base64::engine::general_purpose::URL_SAFE_NO_PAD;

/// What this server says of itself to the servers linked to it: where it is, and the issuer its
/// statements are signed by.
pub fn about(hub: &Hub) -> Value {
    json!({ "server": hub.cfg.server.public_url.trim_end_matches('/'), "issuer": crate::ci::oidc::issuer(hub) })
}

/// What a statement about `app` in `env` is about.
fn subject(app: &str, env: &str) -> String {
    format!("deploy:{env}:{app}")
}

/// A statement of what runs of `app` in `env` here (its release, version, artifacts and since
/// when) and what's on its way there, signed for `audience` (the asking server's issuer). `None`
/// when the app isn't registered there.
pub async fn statement(d: &Deploy, app: &str, env: &str, audience: &str) -> anyhow::Result<Option<String>> {
    let st = &d.hub.store;
    let Some(reg) = store::registration(st, app, env).await? else { return Ok(None) };
    let running = match reg.running {
        Some(id) => store::release(st, id).await?,
        None => None,
    };
    let coming = store::in_flight(st, app, env).await?;
    let now = crate::util::now_unix();
    let mut claims = Map::new();
    claims.insert("iss".into(), json!(crate::ci::oidc::issuer(&d.hub)));
    claims.insert("sub".into(), json!(subject(app, env)));
    claims.insert("aud".into(), json!(audience));
    claims.insert("iat".into(), json!(now));
    claims.insert("nbf".into(), json!(now));
    claims.insert("exp".into(), json!(now + LIFETIME_SECS));
    claims.insert("jti".into(), json!(crate::util::new_id()));
    claims.insert("server".into(), json!(d.hub.cfg.server.public_url.trim_end_matches('/')));
    claims.insert("app".into(), json!(app));
    claims.insert("environment".into(), json!(env));
    claims.insert("running".into(), running.map_or(Value::Null, |r| json!({ "release": r.id, "version": r.version, "artifacts": r.artifacts, "since": r.ended_at.unwrap_or(r.created_at) })));
    claims.insert("coming".into(), coming.map_or(Value::Null, |r| json!({ "release": r.id, "version": r.version, "state": r.state.as_str() })));
    Ok(Some(crate::ci::oidc::mint(&d.hub, &claims).await?))
}

/// Why a linked server's word can't be taken now.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Unsure {
    /// It couldn't be asked, or its issuer's keys read: asked again soon.
    Unreachable(String),
    /// What it answered can't be taken.
    Refused(String),
}

/// A linked server, reached with its integration's token.
pub struct Link {
    client: Client,
    base: String,
    token: String,
    /// The integration's name: how this server's environments name it (`staging@<name>`).
    pub name: String,
    /// The issuer its statements are signed by, when the integration pins one.
    issuer: Option<String>,
}

impl Link {
    /// The linked server an integration of kind `kiln` is.
    pub async fn of(hub: &Hub, name: &str) -> Result<Self, String> {
        let i = crate::ci::integrations::get(hub, name).await.map_err(|e| format!("{e:#}"))?.ok_or_else(|| format!("no linked server {name} (an integration of kind {KIND})"))?;
        if i.kind != KIND {
            return Err(format!("{name} is a {} integration, not a linked kiln server", i.kind));
        }
        let field = |k: &str| i.config.get(k).and_then(Value::as_str).map(str::trim).filter(|s| !s.is_empty()).map(str::to_owned);
        let base = field("url").ok_or_else(|| format!("{name} has no url"))?.trim_end_matches('/').to_owned();
        let token = crate::ci::integrations::token(hub, &i).await.map_err(|e| format!("{e:#}"))?.ok_or_else(|| format!("{name} has no token stored"))?;
        let client = match field("ca") {
            Some(pem) => Client::new(Roots::Pem(pem.into_bytes()), None),
            None => Client::public(),
        }
        .map_err(|e| format!("{e:#}"))?;
        Ok(Self { client, base, token, name: name.to_owned(), issuer: field("issuer").map(|s| s.trim_end_matches('/').to_owned()) })
    }

    async fn get(&self, path: &str) -> Result<(u16, Value), String> {
        let resp = self.client.request(Method::Get, &format!("{}{path}", self.base)).bearer(&self.token).timeout(std::time::Duration::from_secs(20)).send().await.map_err(|e| format!("{} at {}: {e:#}", self.name, self.base))?;
        let status = resp.status();
        let bytes = resp.bytes(1 << 20).await.map_err(|e| format!("{}: {e:#}", self.name))?;
        Ok((status, serde_json::from_slice(&bytes).unwrap_or_else(|_| Value::String(String::from_utf8_lossy(&bytes).into_owned()))))
    }

    /// What it answered that wasn't what was asked for: refused when asking again won't change it.
    fn unsure(&self, status: u16, v: &Value) -> Unsure {
        let said = answered(&self.name, status, v);
        match status {
            401 | 403 => Unsure::Refused(format!("{said}: its token may not read releases there (runs.read)")),
            404 => Unsure::Refused(said),
            _ => Unsure::Unreachable(said),
        }
    }

    /// Where it says it is and the issuer it signs with, its token working.
    pub async fn about(&self) -> Result<(String, String), Unsure> {
        match self.get("/api/deploy/link").await.map_err(Unsure::Unreachable)? {
            (200, v) => {
                let field = |k: &str| v.get(k).and_then(Value::as_str).map(str::to_owned).ok_or_else(|| Unsure::Refused(format!("{} didn't say its {k}", self.name)));
                Ok((field("server")?, field("issuer")?))
            }
            (status, v) => Err(self.unsure(status, &v)),
        }
    }

    /// The issuer its statements must be signed by: the one its integration pins, else the one it
    /// says.
    async fn issuer(&self) -> Result<String, Unsure> {
        match &self.issuer {
            Some(i) => Ok(i.clone()),
            None => self.about().await.map(|(_, issuer)| issuer),
        }
    }

    /// What runs of `app` in its environment `env`, as it signs it for this server: the claims,
    /// checked, and the statement itself.
    pub async fn running(&self, hub: &Hub, app: &str, env: &str) -> Result<(Value, String), Unsure> {
        let issuer = self.issuer().await?;
        let audience = crate::ci::oidc::issuer(hub);
        let path = format!("/api/deploy/apps/{}/environments/{}/statement?audience={}", enc(app), enc(env), enc(&audience));
        let token = match self.get(&path).await.map_err(Unsure::Unreachable)? {
            (200, v) => v.get("statement").and_then(Value::as_str).map(str::to_owned).ok_or_else(|| Unsure::Refused(format!("{} answered without a statement", self.name)))?,
            (status, v) => return Err(self.unsure(status, &v)),
        };
        let claims = verify(&self.client, &token, &issuer, &audience, crate::util::now_unix()).await?;
        if claims.get("sub").and_then(Value::as_str) != Some(subject(app, env).as_str()) {
            return Err(Unsure::Refused(format!("{}'s statement is about {}, not {app} in {env}", self.name, claims.get("sub").and_then(Value::as_str).unwrap_or("nothing"))));
        }
        Ok((claims, token))
    }
}

fn enc(s: &str) -> String {
    kiln_cd::kube::enc(s)
}

fn answered(name: &str, status: u16, v: &Value) -> String {
    let why = v.get("error").or_else(|| v.get("message")).and_then(Value::as_str).map_or_else(|| v.to_string(), str::to_owned);
    format!("{name} answered {status}: {why}")
}

fn part(s: &str) -> Result<Value, Unsure> {
    let bytes = B64.decode(s).map_err(|_| Unsure::Refused("a statement that isn't a JWT".into()))?;
    serde_json::from_slice(&bytes).map_err(|_| Unsure::Refused("a statement that isn't a JWT".into()))
}

/// An issuer's published keys, found through its discovery document.
async fn keys(client: &Client, issuer: &str) -> Result<Value, String> {
    let fetch = |url: String| async move {
        let resp = client.request(Method::Get, &url).timeout(std::time::Duration::from_secs(20)).send().await.map_err(|e| format!("{url}: {e:#}"))?;
        let status = resp.status();
        let bytes = resp.bytes(1 << 20).await.map_err(|e| format!("{url}: {e:#}"))?;
        if status != 200 {
            return Err(format!("{url} answered {status}"));
        }
        serde_json::from_slice::<Value>(&bytes).map_err(|e| format!("{url}: {e}"))
    };
    let discovery = fetch(format!("{issuer}/.well-known/openid-configuration")).await?;
    if discovery.get("issuer").and_then(Value::as_str).map(|i| i.trim_end_matches('/')) != Some(issuer) {
        return Err(format!("{issuer}'s discovery document names another issuer"));
    }
    let jwks = discovery.get("jwks_uri").and_then(Value::as_str).map_or_else(|| format!("{issuer}/jwks"), str::to_owned);
    fetch(jwks).await
}

/// The claims of `token` when `issuer` signed it (RS256, with a key its published set holds) for
/// `audience`, and it holds at `now`.
pub async fn verify(client: &Client, token: &str, issuer: &str, audience: &str, now: i64) -> Result<Value, Unsure> {
    let mut parts = token.split('.');
    let (Some(head), Some(body), Some(sig), None) = (parts.next(), parts.next(), parts.next(), parts.next()) else {
        return Err(Unsure::Refused("a statement that isn't a JWT".into()));
    };
    let header = part(head)?;
    if header.get("alg").and_then(Value::as_str) != Some("RS256") {
        return Err(Unsure::Refused("a statement not signed with RS256".into()));
    }
    let kid = header.get("kid").and_then(Value::as_str).unwrap_or_default().to_owned();
    let claims = part(body)?;
    let said = |k: &str| claims.get(k).and_then(Value::as_str).unwrap_or_default().trim_end_matches('/').to_owned();
    if said("iss") != issuer {
        return Err(Unsure::Refused(format!("a statement signed by {}, not {issuer}", said("iss"))));
    }
    if said("aud") != audience.trim_end_matches('/') {
        return Err(Unsure::Refused(format!("a statement for {}, not this server ({audience})", said("aud"))));
    }
    let at = |k: &str| claims.get(k).and_then(Value::as_i64);
    if at("exp").is_none_or(|exp| exp + LEEWAY_SECS < now) {
        return Err(Unsure::Refused("a statement that no longer holds (expired)".into()));
    }
    if at("nbf").is_some_and(|nbf| nbf - LEEWAY_SECS > now) {
        return Err(Unsure::Refused("a statement that doesn't hold yet".into()));
    }
    let set = keys(client, issuer).await.map_err(|e| Unsure::Unreachable(format!("{issuer}'s keys can't be read: {e}")))?;
    let key = set.get("keys").and_then(Value::as_array).into_iter().flatten().find(|k| k.get("kid").and_then(Value::as_str) == Some(kid.as_str()));
    let Some(key) = key else {
        return Err(Unsure::Refused(format!("{issuer} publishes no key {kid}")));
    };
    let decode = |k: &str| key.get(k).and_then(Value::as_str).and_then(|s| B64.decode(s).ok()).ok_or_else(|| Unsure::Refused(format!("{issuer}'s key {kid} doesn't read")));
    let (n, e) = (decode("n")?, decode("e")?);
    let sig = B64.decode(sig).map_err(|_| Unsure::Refused("a statement whose signature doesn't read".into()))?;
    let public = aws_lc_rs::signature::RsaPublicKeyComponents { n: n.as_slice(), e: e.as_slice() };
    public
        .verify(&aws_lc_rs::signature::RSA_PKCS1_2048_8192_SHA256, format!("{head}.{body}").as_bytes(), &sig)
        .map_err(|_| Unsure::Refused(format!("a statement whose signature doesn't verify with {issuer}'s key {kid}")))?;
    Ok(claims)
}

/// The integration's check: the server answers with its token, and its issuer's keys are there to
/// check what it signs.
pub async fn check(hub: &Hub, name: &str) -> Result<String, String> {
    let link = Link::of(hub, name).await?;
    let (server, said) = link.about().await.map_err(|(Unsure::Unreachable(why) | Unsure::Refused(why))| why)?;
    let issuer = link.issuer.clone().unwrap_or(said);
    let set = keys(&link.client, &issuer).await.map_err(|e| format!("{server} signs with {issuer}, whose keys can't be read: {e}"))?;
    let n = set.get("keys").and_then(Value::as_array).map_or(0, Vec::len);
    Ok(format!("{server} answers; it signs with {issuer} ({n} key{} published)", if n == 1 { "" } else { "s" }))
}
