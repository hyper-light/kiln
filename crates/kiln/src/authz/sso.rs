//! SSO with GitHub, GitLab and Bitbucket (docs/ci.md W23-R08), beside kiln's own sign-in (W10):
//! OAuth 2's authorization code flow, then the host's API says who the person is (a stable id, a
//! verified email) and what they belong to (GitHub's organizations and teams, GitLab's groups,
//! Bitbucket's workspaces).
//! - **Who may sign in**: anyone the host signs in, or only members of `allowed`.
//! - **Which kiln user**: the one linked to that account, else the one with that verified email,
//!   else a new one when `create_users` allows it.
//! - **What they may do**: `grants` by membership, given at each sign-in and taken back when the
//!   membership is gone (grants of kiln's own stay as they are).
//! - **Acting as themselves**: the host's token is kept (sealed) beside the linked account, for
//!   comments and reviews made from kiln as that person.

use http::StatusCode;
use rusqlite::{OptionalExtension as _, params};
use serde_json::{Value, json};

use super::Grant;
use crate::ci::Fail;
use crate::config::sso::Provider;
use crate::http::client::Method;
use crate::hub::Hub;

fn fail(e: anyhow::Error) -> Fail {
    (StatusCode::INTERNAL_SERVER_ERROR, format!("{e:#}"))
}

fn bad(m: impl Into<String>) -> Fail {
    (StatusCode::FORBIDDEN, m.into())
}

/// A host kiln signs people in with.
struct Client {
    provider: &'static str,
    client_id: String,
    client_secret: String,
    authorize: String,
    token: String,
    api: String,
    cfg: Provider,
}

const TOKEN_AAD: &[u8] = b"sso-host-token";

fn redirect_uri(hub: &Hub, provider: &str) -> String {
    format!("{}/api/auth/{provider}/callback", hub.cfg.server.public_url.trim_end_matches('/'))
}

fn read_secret(file: Option<&std::path::PathBuf>) -> Option<String> {
    std::fs::read_to_string(file?).ok().map(|s| s.trim().to_owned()).filter(|s| !s.is_empty())
}

/// The App's OAuth client (GitHub's manifest flow kept it): id and secret.
async fn app_client(hub: &Hub) -> Option<(String, String)> {
    let row: Option<(Option<String>, Option<Vec<u8>>, Option<Vec<u8>>)> = hub.store.call(|c| Ok(c.query_row("SELECT client_id, client_secret_sealed, client_secret_nonce FROM github_app WHERE id = 1", [], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?))).optional()?)).await.ok().flatten();
    let (Some(id), Some(s), Some(n)) = row? else { return None };
    let secret = crate::ci::secrets::open_master(hub, &s, &n, b"github-app-client-secret").ok()?;
    Some((id, String::from_utf8(secret).ok()?))
}

async fn client(hub: &Hub, provider: &str) -> Option<Client> {
    let sso = &hub.cfg.sso;
    match provider {
        "github" => {
            let cfg = sso.github.clone().unwrap_or_default();
            let (id, secret) = match (cfg.client_id.clone(), read_secret(cfg.client_secret_file.as_ref())) {
                (Some(i), Some(s)) => (i, s),
                // Nothing configured, but an App the manifest flow made: its OAuth client.
                _ if sso.github.is_some() || hub.cfg.ci.github.app_id.is_none() => app_client(hub).await?,
                _ => return None,
            };
            let web = cfg.web_url.clone().unwrap_or_else(|| hub.cfg.ci.github.web_url.clone()).trim_end_matches('/').to_owned();
            let api = cfg.api_url.clone().unwrap_or_else(|| hub.cfg.ci.github.api_url.clone()).trim_end_matches('/').to_owned();
            Some(Client { provider: "github", client_id: id, client_secret: secret, authorize: format!("{web}/login/oauth/authorize"), token: format!("{web}/login/oauth/access_token"), api, cfg })
        }
        "gitlab" => {
            let cfg = sso.gitlab.clone()?;
            let url = cfg.url.clone().unwrap_or_else(|| "https://gitlab.com".into()).trim_end_matches('/').to_owned();
            Some(Client { provider: "gitlab", client_id: cfg.client_id.clone()?, client_secret: read_secret(cfg.client_secret_file.as_ref())?, authorize: format!("{url}/oauth/authorize"), token: format!("{url}/oauth/token"), api: format!("{url}/api/v4"), cfg })
        }
        "bitbucket" => {
            let cfg = sso.bitbucket.clone()?;
            let web = cfg.web_url.clone().unwrap_or_else(|| "https://bitbucket.org".into()).trim_end_matches('/').to_owned();
            let api = cfg.api_url.clone().unwrap_or_else(|| "https://api.bitbucket.org/2.0".into()).trim_end_matches('/').to_owned();
            Some(Client { provider: "bitbucket", client_id: cfg.client_id.clone()?, client_secret: read_secret(cfg.client_secret_file.as_ref())?, authorize: format!("{web}/site/oauth2/authorize"), token: format!("{web}/site/oauth2/access_token"), api, cfg })
        }
        _ => None,
    }
}

/// `GET /api/auth/sso`: the hosts people may sign in with here, for the sign-in page.
pub async fn providers(hub: &Hub) -> Value {
    let mut out = Vec::new();
    for (p, name) in [("github", "GitHub"), ("gitlab", "GitLab"), ("bitbucket", "Bitbucket")] {
        if client(hub, p).await.is_some() {
            out.push(json!({ "provider": p, "name": name, "start": format!("/api/auth/{p}/start") }));
        }
    }
    json!({ "providers": out })
}

/// A local path to come back to (never another site).
fn local(ret: Option<&str>) -> String {
    match ret {
        Some(r) if r.starts_with('/') && !r.starts_with("//") && !r.contains('\\') => r.to_owned(),
        _ => "/".to_owned(),
    }
}

/// `GET /api/auth/:provider/start?return=/path`: where the browser goes to sign in.
pub async fn start(hub: &Hub, provider: &str, ret: Option<&str>) -> Result<String, Fail> {
    let c = client(hub, provider).await.ok_or((StatusCode::NOT_FOUND, format!("signing in with {provider} isn't set up here")))?;
    let state = crate::util::random_token();
    let (st, p, back) = (state.clone(), c.provider.to_owned(), local(ret));
    hub.store
        .call(move |c| {
            let now = crate::util::now_unix();
            c.execute("DELETE FROM sso_states WHERE created_at < ?1", [now - 600])?;
            c.execute("INSERT INTO sso_states (state, provider, return_to, created_at) VALUES (?1, ?2, ?3, ?4)", params![st, p, back, now])?;
            Ok(())
        })
        .await
        .map_err(fail)?;
    let enc = crate::ci::cli::enc;
    let uri = enc(&redirect_uri(hub, provider));
    Ok(match c.provider {
        "github" => format!("{}?client_id={}&redirect_uri={uri}&state={state}&scope=read:org%20user:email", c.authorize, enc(&c.client_id)),
        "gitlab" => format!("{}?client_id={}&redirect_uri={uri}&response_type=code&state={state}&scope=read_user%20read_api", c.authorize, enc(&c.client_id)),
        _ => format!("{}?client_id={}&response_type=code&state={state}", c.authorize, enc(&c.client_id)),
    })
}

/// Who the host says the person is.
#[derive(Debug, Clone, Default)]
pub struct Identity {
    pub subject: String,
    pub login: String,
    pub email: Option<String>,
    /// Organizations and `org/team`s (GitHub), groups' full paths (GitLab), workspaces (Bitbucket).
    pub groups: Vec<String>,
}

async fn call(method: Method, url: &str, bearer: Option<&str>, basic: Option<(&str, &str)>, form: Option<String>) -> anyhow::Result<(u16, Value)> {
    let client = crate::http::client::github()?;
    let mut req = client.request(method, url).header("Accept", "application/json").header("User-Agent", "kiln").timeout(std::time::Duration::from_secs(30));
    if let Some(t) = bearer {
        req = req.bearer(t);
    }
    if let Some((u, p)) = basic {
        use base64::Engine as _;
        req = req.header("Authorization", &format!("Basic {}", base64::engine::general_purpose::STANDARD.encode(format!("{u}:{p}"))));
    }
    if let Some(f) = form {
        req = req.header("Content-Type", "application/x-www-form-urlencoded").body(f);
    }
    let resp = req.send().await?;
    let status = resp.status();
    let bytes = resp.bytes(8 << 20).await?;
    Ok((status, serde_json::from_slice(&bytes).unwrap_or(Value::Null)))
}

/// The code GitHub, GitLab or Bitbucket sent back, as an access token.
async fn exchange(hub: &Hub, c: &Client, code: &str) -> anyhow::Result<String> {
    let enc = crate::ci::cli::enc;
    let uri = redirect_uri(hub, c.provider);
    let (status, v) = match c.provider {
        "bitbucket" => call(Method::Post, &c.token, None, Some((&c.client_id, &c.client_secret)), Some(format!("grant_type=authorization_code&code={}", enc(code)))).await?,
        _ => {
            let form = format!("client_id={}&client_secret={}&code={}&redirect_uri={}&grant_type=authorization_code", enc(&c.client_id), enc(&c.client_secret), enc(code), enc(&uri));
            call(Method::Post, &c.token, None, None, Some(form)).await?
        }
    };
    v.get("access_token").and_then(Value::as_str).map(str::to_owned).ok_or_else(|| anyhow::anyhow!("{} didn't sign the person in ({status}): {}", c.provider, v.get("error_description").or_else(|| v.get("error")).cloned().unwrap_or(v)))
}

async fn identity(c: &Client, token: &str) -> anyhow::Result<Identity> {
    let get = |path: String| {
        let url = format!("{}{path}", c.api);
        async move {
            let (st, v) = call(Method::Get, &url, Some(token), None, None).await?;
            anyhow::ensure!(st == 200, "{url}: {st}");
            anyhow::Ok(v)
        }
    };
    let s = |v: &Value, k: &str| v.get(k).and_then(Value::as_str).map(str::to_owned);
    match c.provider {
        "github" => {
            let me = get("/user".into()).await?;
            let emails = get("/user/emails".into()).await.unwrap_or(Value::Null);
            let email = emails.as_array().and_then(|l| l.iter().find(|e| e.get("primary") == Some(&json!(true)) && e.get("verified") == Some(&json!(true)))).and_then(|e| s(e, "email"));
            let mut groups: Vec<String> = get("/user/orgs".into()).await.unwrap_or(Value::Null).as_array().into_iter().flatten().filter_map(|o| s(o, "login")).collect();
            for t in get("/user/teams".into()).await.unwrap_or(Value::Null).as_array().into_iter().flatten() {
                if let (Some(org), Some(slug)) = (t.pointer("/organization/login").and_then(Value::as_str), s(t, "slug")) {
                    groups.push(format!("{org}/{slug}"));
                }
            }
            Ok(Identity { subject: me.get("id").map(Value::to_string).unwrap_or_default(), login: s(&me, "login").unwrap_or_default(), email, groups })
        }
        "gitlab" => {
            let me = get("/user".into()).await?;
            // GitLab's `email` is the primary one, confirmed when `confirmed_at` says so.
            let email = me.get("confirmed_at").filter(|v| !v.is_null()).and_then(|_| s(&me, "email"));
            let groups = get("/groups?min_access_level=10&per_page=100".into()).await.unwrap_or(Value::Null).as_array().into_iter().flatten().filter_map(|g| s(g, "full_path")).collect();
            Ok(Identity { subject: me.get("id").map(Value::to_string).unwrap_or_default(), login: s(&me, "username").unwrap_or_default(), email, groups })
        }
        _ => {
            let me = get("/user".into()).await?;
            let emails = get("/user/emails".into()).await.unwrap_or(Value::Null);
            let email = emails.get("values").and_then(Value::as_array).and_then(|l| l.iter().find(|e| e.get("is_primary") == Some(&json!(true)) && e.get("is_confirmed") == Some(&json!(true)))).and_then(|e| s(e, "email"));
            let groups = get("/user/permissions/workspaces?pagelen=100".into()).await.unwrap_or(Value::Null).get("values").and_then(Value::as_array).into_iter().flatten().filter_map(|w| w.pointer("/workspace/slug").and_then(Value::as_str).map(str::to_owned)).collect();
            Ok(Identity { subject: s(&me, "uuid").unwrap_or_default(), login: s(&me, "username").or_else(|| s(&me, "nickname")).unwrap_or_default(), email, groups })
        }
    }
}

/// Whether a membership (`acme`, `acme/platform`) is `want` or under it.
fn belongs(groups: &[String], want: &str) -> bool {
    groups.iter().any(|g| g.eq_ignore_ascii_case(want) || g.to_ascii_lowercase().starts_with(&format!("{}/", want.to_ascii_lowercase())))
}

/// The grants a person's memberships give by `grants` (exact memberships only: a team's grants
/// need the team).
pub fn mapped(cfg: &Provider, groups: &[String]) -> Vec<String> {
    let mut out: Vec<String> = cfg.grants.iter().filter(|(k, _)| groups.iter().any(|g| g.eq_ignore_ascii_case(k))).flat_map(|(_, v)| v.clone()).collect();
    out.sort();
    out.dedup();
    out
}

/// A user name for a new account: the login, made unique.
fn name_for(c: &rusqlite::Connection, login: &str, provider: &str) -> rusqlite::Result<String> {
    let base: String = login.chars().filter(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.')).collect();
    let base = if base.is_empty() { format!("{provider}-user") } else { base };
    for n in 0..100 {
        let name = if n == 0 { base.clone() } else { format!("{base}-{provider}{}", if n == 1 { String::new() } else { n.to_string() }) };
        let taken: bool = c.query_row("SELECT EXISTS(SELECT 1 FROM users WHERE name = ?1)", [&name], |r| r.get(0))?;
        if !taken {
            return Ok(name);
        }
    }
    Ok(format!("{base}-{}", crate::util::new_id()))
}

/// `GET /api/auth/:provider/callback?code&state`: the host, back with the person. Their kiln
/// session's value, their name, and where to go.
pub async fn callback(hub: &Hub, provider: &str, code: &str, state: &str) -> Result<(String, String, String), Fail> {
    let c = client(hub, provider).await.ok_or((StatusCode::NOT_FOUND, format!("signing in with {provider} isn't set up here")))?;
    let (st, p) = (state.to_owned(), c.provider.to_owned());
    let back: Option<String> = hub
        .store
        .call(move |c| {
            let r: Option<String> = c.query_row("SELECT return_to FROM sso_states WHERE state = ?1 AND provider = ?2 AND created_at > ?3", params![st, p, crate::util::now_unix() - 600], |r| r.get(0)).optional()?;
            c.execute("DELETE FROM sso_states WHERE state = ?1", [&st])?;
            Ok(r)
        })
        .await
        .map_err(fail)?;
    let back = back.ok_or_else(|| bad("this sign-in wasn't started here, or took over ten minutes: start again"))?;
    let token = exchange(hub, &c, code).await.map_err(|e| bad(format!("{e:#}")))?;
    let who = identity(&c, &token).await.map_err(|e| (StatusCode::BAD_GATEWAY, format!("{e:#}")))?;
    if who.subject.is_empty() || who.subject == "null" {
        return Err(bad(format!("{provider} didn't say who signed in")));
    }
    if !c.cfg.allowed.is_empty() && !c.cfg.allowed.iter().any(|a| belongs(&who.groups, a)) {
        hub.store.audit(format!("{provider}:{}", who.login), "login.refused".into(), format!("not a member of {}", c.cfg.allowed.join(", "))).await;
        return Err(bad(format!("{} isn't a member of {} on {provider}", who.login, c.cfg.allowed.join(", "))));
    }
    let grants = mapped(&c.cfg, &who.groups);
    for g in &grants {
        Grant::parse(g).map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, format!("[sso.{provider}] grants: {e}")))?;
    }
    let (sealed, nonce) = crate::ci::secrets::seal_master(hub, token.as_bytes(), TOKEN_AAD).map_err(fail)?;
    let (p, create) = (c.provider.to_owned(), c.cfg.create_users);
    let w = who.clone();
    let found: Result<(String, String, i64), String> = hub
        .store
        .call(move |c| {
            let tx = c.transaction()?;
            let now = crate::util::now_unix();
            // The linked account, else the verified email, else a new user (when allowed).
            let mut user: Option<String> = tx.query_row("SELECT user_id FROM user_identities WHERE provider = ?1 AND subject = ?2", params![p, w.subject], |r| r.get(0)).optional()?;
            if user.is_none()
                && let Some(e) = &w.email
            {
                user = tx.query_row("SELECT id FROM users WHERE email = ?1 COLLATE NOCASE AND disabled_at IS NULL", [e], |r| r.get(0)).optional()?;
            }
            let user = match user {
                Some(u) => u,
                None if create => {
                    let id = crate::util::new_id();
                    let name = name_for(&tx, &w.login, &p)?;
                    tx.execute("INSERT INTO users (id, name, created_at, created_by, email) VALUES (?1, ?2, ?3, ?4, ?5)", params![id, name, now, format!("sso:{p}"), w.email])?;
                    id
                }
                None => return Ok(Err(format!("{} has no kiln user here: an administrator adds you (with your email), or lets {p} sign people up", w.login))),
            };
            let row: Option<(String, i64)> = tx.query_row("SELECT name, session_epoch FROM users WHERE id = ?1 AND disabled_at IS NULL", [&user], |r| Ok((r.get(0)?, r.get(1)?))).optional()?;
            let Some((name, epoch)) = row else { return Ok(Err("this kiln user is disabled".to_owned())) };
            tx.execute(
                "INSERT INTO user_identities (provider, subject, user_id, login, email, groups, token_sealed, token_nonce, created_at, last_at) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?9)
                 ON CONFLICT(provider, subject) DO UPDATE SET user_id = excluded.user_id, login = excluded.login, email = excluded.email, groups = excluded.groups,
                   token_sealed = excluded.token_sealed, token_nonce = excluded.token_nonce, last_at = excluded.last_at",
                params![p, w.subject, user, w.login, w.email, serde_json::to_string(&w.groups).unwrap_or_default(), sealed, nonce, now],
            )?;
            // Membership's grants, as they are now.
            let source = format!("sso:{p}");
            tx.execute("DELETE FROM grants WHERE principal = ?1 AND created_by = ?2", params![format!("user:{user}"), source])?;
            for g in &grants {
                if let Ok(g) = Grant::parse(g) {
                    tx.execute("INSERT INTO grants (principal, what, scope, deny, created_at, created_by) VALUES (?1, ?2, ?3, ?4, ?5, ?6)", params![format!("user:{user}"), g.what, g.scope.text(), g.deny, now, source])?;
                }
            }
            tx.commit()?;
            Ok(Ok((user, name, epoch)))
        })
        .await
        .map_err(fail)?;
    let (user, name, epoch) = found.map_err(bad)?;
    hub.store.audit(format!("user:{name}"), "login".into(), format!("{provider} as {} ({} grants by membership)", who.login, mapped(&c.cfg, &who.groups).len())).await;
    Ok((super::gate::session_value(hub, &user, epoch), name, back))
}

/// The host's token of a person who signed in with it (for acting on the host as them).
pub async fn token_of(hub: &Hub, provider: &str, user_id: &str) -> Option<String> {
    let (p, u) = (provider.to_owned(), user_id.to_owned());
    let row: Option<(Vec<u8>, Vec<u8>)> = hub.store.call(move |c| Ok(c.query_row("SELECT token_sealed, token_nonce FROM user_identities WHERE provider = ?1 AND user_id = ?2 ORDER BY last_at DESC LIMIT 1", params![p, u], |r| Ok((r.get(0)?, r.get(1)?))).optional()?)).await.ok().flatten();
    let (s, n) = row?;
    String::from_utf8(crate::ci::secrets::open_master(hub, &s, &n, TOKEN_AAD).ok()?).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn w23_r08_memberships() {
        let groups = vec!["acme".to_owned(), "acme/platform".to_owned(), "other".to_owned()];
        assert!(belongs(&groups, "acme") && belongs(&["acme/platform".to_owned()], "acme") && !belongs(&groups, "acm"));
        let cfg = Provider { grants: [("acme/platform".to_owned(), vec!["developer@org:acme".to_owned()]), ("acme".to_owned(), vec!["viewer@org:acme".to_owned()]), ("nope".to_owned(), vec!["owner".to_owned()])].into(), ..Provider::default() };
        assert_eq!(mapped(&cfg, &groups), ["developer@org:acme", "viewer@org:acme"]);
        assert_eq!(mapped(&cfg, &["acme".to_owned()]), ["viewer@org:acme"], "a team's grants need the team");
        assert_eq!(local(Some("/runs?x=1")), "/runs?x=1");
        assert_eq!((local(Some("//evil.example")), local(Some("https://evil.example")), local(None)), ("/".to_owned(), "/".to_owned(), "/".to_owned()));
    }
}
