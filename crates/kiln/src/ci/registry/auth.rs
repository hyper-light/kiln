//! Who may pull and push (docs/ci.md W12-R01): Docker's token authentication. A client asks
//! `/v2/token` (with a PAT or a job token as Basic's password, or as a bearer token) for the
//! scopes it wants (`repository:acme/app:pull,push`), and gets a short-lived registry token for
//! those it may have; a request with no credentials gets the challenge that says where to ask.
//! Registry requests may also carry a PAT or a job token themselves.
//!
//! - **Pull** is `artifacts.read`, **push** and **delete** are `artifacts.write`, where the
//!   name lives: `acme/app` and `acme/app/web` in `repo:acme/app`, a one-part name in its org.
//!   A job's token pulls where `packages: read` (or `actions: read`) lets it, and pushes where
//!   `packages: write` does, in its own repository's names.
//! - **Anonymous pull** is opt-in, per namespace (`[ci.registry] anonymous_pull`), or per mirror
//!   (`anonymous`: Docker's `registry-mirrors` pull without signing in).
//! - **A mirror** (W12-R02) is pulled from by anyone signed in, and pushed to by no one.

use base64::Engine as _;
use serde::{Deserialize, Serialize};

use crate::authz::{Caller, Scope};
use crate::hub::Hub;

const B64URL: base64::engine::GeneralPurpose = base64::engine::general_purpose::URL_SAFE_NO_PAD;
const B64: base64::engine::GeneralPurpose = base64::engine::general_purpose::STANDARD;

/// How long a registry token lasts.
pub const TOKEN_SECS: i64 = 300;

/// What one may do with a name.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Action {
    Pull,
    Push,
    Delete,
}

impl Action {
    fn parse(s: &str) -> Option<Self> {
        match s {
            "pull" => Some(Self::Pull),
            "push" => Some(Self::Push),
            "delete" => Some(Self::Delete),
            _ => None,
        }
    }

    pub fn word(self) -> &'static str {
        match self {
            Self::Pull => "pull",
            Self::Push => "push",
            Self::Delete => "delete",
        }
    }

    fn capability(self) -> &'static str {
        match self {
            Self::Pull => "artifacts.read",
            Self::Push | Self::Delete => "artifacts.write",
        }
    }
}

/// Granted access: names (or `*`: the catalog) and what may be done with each.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct Access {
    /// Who: `user:ada`, `job:<leg>`, `anonymous`.
    pub sub: String,
    pub grants: Vec<(String, Vec<Action>)>,
    pub exp: i64,
}

impl Access {
    pub fn allows(&self, name: &str, action: Action) -> bool {
        self.grants.iter().any(|(n, acts)| (n == name || n == "*") && acts.contains(&action))
    }
}

fn key(hub: &Hub) -> String {
    crate::util::hmac_hex(hub.admin_token.as_bytes(), b"kiln registry tokens v1")
}

/// A registry token for `access`: its claims and their signature.
pub fn token(hub: &Hub, access: &Access) -> String {
    let body = B64URL.encode(serde_json::to_vec(access).unwrap_or_default());
    let sig = crate::util::hmac_hex(key(hub).as_bytes(), body.as_bytes());
    format!("kreg.{body}.{sig}")
}

/// The access a registry token carries, if it is one of this server's and hasn't expired.
pub fn verify(hub: &Hub, t: &str) -> Option<Access> {
    let rest = t.strip_prefix("kreg.")?;
    let (body, sig) = rest.rsplit_once('.')?;
    let want = crate::util::hmac_hex(key(hub).as_bytes(), body.as_bytes());
    if !crate::util::ct_eq(want.as_bytes(), sig.as_bytes()) {
        return None;
    }
    let a: Access = serde_json::from_slice(&B64URL.decode(body).ok()?).ok()?;
    (a.exp > crate::util::now_unix()).then_some(a)
}

/// The scope a name's rights are checked in: its repository's for `owner/name[/…]`, its org's for
/// a one-part name.
pub fn scope_of(name: &str) -> Scope {
    let mut parts = name.splitn(3, '/');
    match (parts.next(), parts.next()) {
        (Some(o), Some(r)) => Scope::Repo { repo: format!("{o}/{r}") },
        (Some(o), None) => Scope::Org { org: o.to_owned() },
        _ => Scope::Server,
    }
}

/// Whether anyone may pull `name` (`[ci.registry] anonymous_pull` names it or a namespace above, or
/// it's a mirror's that allows it).
pub fn anonymous(hub: &Hub, name: &str) -> bool {
    if super::mirror::of(hub, name).is_some_and(|(m, _)| m.anonymous) {
        return true;
    }
    hub.cfg.ci.registry.anonymous_pull.iter().any(|ns| {
        let ns = ns.trim_matches('/');
        ns == "*" || name == ns || name.strip_prefix(ns).is_some_and(|rest| rest.starts_with('/'))
    })
}

/// What `caller` (none: anonymous) may do with `name`, of `wanted`.
pub fn may(hub: &Hub, caller: Option<&Caller>, name: &str, wanted: &[Action]) -> Vec<Action> {
    wanted
        .iter()
        .copied()
        .filter(|a| match (caller, super::mirror::of(hub, name).is_some()) {
            (Some(_), true) => *a == Action::Pull,
            (Some(c), false) => c.is_owner() || c.can(a.capability(), &scope_of(name)),
            (None, _) => *a == Action::Pull && anonymous(hub, name),
        })
        .collect()
}

/// The scopes a token request asks for (`repository:acme/app:pull,push`, `registry:catalog:*`).
pub fn requested(scopes: &[String]) -> Vec<(String, Vec<Action>)> {
    let mut out: Vec<(String, Vec<Action>)> = Vec::new();
    for s in scopes.iter().flat_map(|s| s.split(' ')) {
        let mut parts = s.splitn(3, ':');
        let (kind, name, acts) = (parts.next().unwrap_or_default(), parts.next().unwrap_or_default(), parts.next().unwrap_or_default());
        let (name, acts) = match kind {
            "repository" => (name.to_owned(), acts.split(',').filter_map(Action::parse).collect::<Vec<_>>()),
            "registry" if name == "catalog" => ("*".to_owned(), vec![Action::Pull]),
            _ => continue,
        };
        match out.iter_mut().find(|(n, _)| *n == name) {
            Some((_, a)) => a.extend(acts.into_iter().filter(|x| !a.contains(x)).collect::<Vec<_>>()),
            None => out.push((name, acts)),
        }
    }
    out
}

/// The access to grant `caller` for a token request's scopes.
pub fn grant(hub: &Hub, caller: Option<&Caller>, scopes: &[String]) -> Access {
    let grants = requested(scopes)
        .into_iter()
        .map(|(name, acts)| {
            // The catalog lists only what each name lets its reader see; asking for it is free.
            let given = if name == "*" { acts } else { may(hub, caller, &name, &acts) };
            (name, given)
        })
        .filter(|(_, a)| !a.is_empty())
        .collect();
    Access { sub: caller.map_or_else(|| "anonymous".to_owned(), |c| c.name.clone()), grants, exp: crate::util::now_unix() + TOKEN_SECS }
}

/// A request's credentials: a registry token, or the secret to resolve to a caller (Basic's
/// password, or a bearer PAT or job token).
pub enum Credentials {
    None,
    Registry(Access),
    Secret(String),
    Invalid,
}

pub fn credentials(hub: &Hub, authorization: Option<&str>) -> Credentials {
    let Some(h) = authorization.map(str::trim).filter(|h| !h.is_empty()) else { return Credentials::None };
    if let Some(t) = h.strip_prefix("Bearer ").map(str::trim) {
        return match verify(hub, t) {
            Some(a) => Credentials::Registry(a),
            None if t.starts_with("kreg.") => Credentials::Invalid,
            None => Credentials::Secret(t.to_owned()),
        };
    }
    if let Some(b) = h.strip_prefix("Basic ").map(str::trim) {
        let decoded = B64.decode(b).ok().and_then(|d| String::from_utf8(d).ok());
        return match decoded.as_deref().and_then(|d| d.split_once(':')) {
            Some((_, secret)) if !secret.is_empty() => Credentials::Secret(secret.to_owned()),
            _ => Credentials::Invalid,
        };
    }
    Credentials::Invalid
}

/// The challenge a request without enough access gets: where to ask for a token, for what.
pub fn challenge(hub: &Hub, scope: Option<(&str, &[Action])>) -> String {
    match scope {
        Some((name, acts)) => challenge_for(hub, &[(name, acts)]),
        None => format!("Bearer realm=\"{}/v2/token\",service=\"kiln\"", hub.cfg.server.public_url.trim_end_matches('/')),
    }
}

/// A challenge for several names' access at once (a mount: the target's push, the source's pull).
pub fn challenge_for(hub: &Hub, scopes: &[(&str, &[Action])]) -> String {
    let realm = format!("{}/v2/token", hub.cfg.server.public_url.trim_end_matches('/'));
    let wanted: Vec<String> = scopes.iter().map(|(name, acts)| format!("repository:{name}:{}", acts.iter().map(|a| a.word()).collect::<Vec<_>>().join(","))).collect();
    format!("Bearer realm=\"{realm}\",service=\"kiln\",scope=\"{}\"", wanted.join(" "))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn w12_r01_scopes() {
        let r = requested(&["repository:acme/app:pull,push".into(), "repository:acme/app:pull repository:acme/lib:pull".into(), "registry:catalog:*".into(), "nonsense".into()]);
        assert_eq!(r, vec![("acme/app".to_owned(), vec![Action::Pull, Action::Push]), ("acme/lib".to_owned(), vec![Action::Pull]), ("*".to_owned(), vec![Action::Pull])]);
        assert_eq!(scope_of("acme/app/web"), Scope::Repo { repo: "acme/app".into() });
        assert_eq!(scope_of("library"), Scope::Org { org: "library".into() });
        let a = Access { sub: "user:ada".into(), grants: vec![("acme/app".into(), vec![Action::Pull])], exp: i64::MAX };
        assert!(a.allows("acme/app", Action::Pull) && !a.allows("acme/app", Action::Push) && !a.allows("acme/lib", Action::Pull));
    }
}
