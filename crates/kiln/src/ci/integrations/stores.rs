//! Secret stores a leg's secrets may come from (docs/ci.md W09-R08, W11): HashiCorp Vault,
//! Infisical, Doppler and 1Password (through a Connect server), beside the clouds' own.
//!
//! - **Vault**: kiln's identity for the leg (an ID token, audience the integration's `audience`,
//!   `vault` by default) at the JWT auth method (`auth/<auth_path>/login` as `role`), then the
//!   secret's key at its path (`vault:secret/data/ci#npm_token`: KV v2's `data.data`, else v1's
//!   `data`).
//! - **Infisical**: a machine identity, by OIDC (kiln's token for `identity_id`) or universal
//!   auth (`<client id>:<client secret>` stored), then a project's secret
//!   (`infisical:<environment>[/<path…>]/<NAME>`).
//! - **Doppler**: a service token stored, or a service account identity by OIDC (`identity`),
//!   then a config's secret (`doppler:<project>/<config>/<NAME>`).
//! - **1Password**: a Connect server's token, then a vault's item's field
//!   (`1password:<vault>/<item>/<field>`, each by name).
//!
//! Each store's address is the administrator's, so requests to it keep to the server's reach
//! (`[ci] allow_private_targets` for one on a private network).

use std::future::Future;
use std::time::Duration;

use anyhow::{Context as _, Result, bail};
use serde_json::{Value, json};

use super::Integration;
use crate::http::io::Reach;
use crate::hub::Hub;

/// The store kinds.
pub const KINDS: &[&str] = &["vault", "infisical", "doppler", "1password"];

fn cfg(i: &Integration, k: &str) -> Option<String> {
    i.config.get(k).and_then(Value::as_str).map(str::trim).filter(|s| !s.is_empty()).map(|s| s.trim_end_matches('/').to_owned())
}

/// One JSON request: its answer, or what the store said.
async fn call(reach: &Reach, method: &str, url: &str, headers: &[(&str, &str)], body: Option<&Value>, who: &str) -> Result<Value> {
    let client = crate::http::client::Client::public()?;
    let mut req = if method == "POST" { client.post(url) } else { client.get(url) };
    req = req.header("accept", "application/json").timeout(Duration::from_secs(20)).reaching(reach);
    for (k, v) in headers {
        req = req.header(k, v);
    }
    if let Some(b) = body {
        req = req.header("content-type", "application/json").body(serde_json::to_vec(b)?);
    }
    let resp = Box::pin(req.send()).await.with_context(|| format!("{who} at {url}"))?;
    let status = resp.status();
    let text = String::from_utf8_lossy(&resp.bytes(1 << 20).await.unwrap_or_default()).into_owned();
    let v: Value = serde_json::from_str(&text).unwrap_or(Value::Null);
    if !(200..300).contains(&status) {
        let said = v.pointer("/errors/0").and_then(Value::as_str).or_else(|| v.get("message").and_then(Value::as_str)).or_else(|| v.get("error").and_then(Value::as_str)).map(str::to_owned).unwrap_or_else(|| crate::util::truncate(text.trim(), 200).to_owned());
        bail!("{who} answered {status}: {said}");
    }
    Ok(v)
}

// ------------------------------------------------------------------ Vault

/// A Vault token for `jwt` at `address`'s JWT auth method.
pub async fn vault_login(reach: &Reach, address: &str, auth_path: &str, role: &str, jwt: &str, namespace: Option<&str>) -> Result<String> {
    let headers: Vec<(&str, &str)> = namespace.map(|n| ("x-vault-namespace", n)).into_iter().collect();
    let v = call(reach, "POST", &format!("{address}/v1/auth/{auth_path}/login"), &headers, Some(&json!({ "role": role, "jwt": jwt })), "Vault").await?;
    v.pointer("/auth/client_token").and_then(Value::as_str).map(str::to_owned).context("Vault gave no token")
}

/// `key` of the secret at `path` (KV v2's `data.data`, else KV v1's `data`).
pub async fn vault_read(reach: &Reach, address: &str, token: &str, path: &str, key: &str, namespace: Option<&str>) -> Result<String> {
    let mut headers: Vec<(&str, &str)> = vec![("x-vault-token", token)];
    if let Some(n) = namespace {
        headers.push(("x-vault-namespace", n));
    }
    let v = call(reach, "GET", &format!("{address}/v1/{}", path.trim_start_matches('/')), &headers, None, "Vault").await?;
    let data = v.pointer("/data/data").filter(|d| d.is_object()).or_else(|| v.get("data")).context("Vault's answer has no data")?;
    match data.get(key) {
        Some(Value::String(s)) => Ok(s.clone()),
        Some(other) => Ok(other.to_string()),
        None => bail!("{path} has no `{key}`"),
    }
}

// ------------------------------------------------------------------ Infisical

/// An Infisical access token: universal auth's `<client id>:<client secret>`, else OIDC with
/// kiln's token for `identity`.
pub async fn infisical_login(reach: &Reach, site: &str, secret: Option<&str>, identity: Option<&str>, jwt: impl Future<Output = Result<String>>) -> Result<String> {
    let v = match secret.and_then(|s| s.split_once(':')) {
        Some((id, sec)) => call(reach, "POST", &format!("{site}/api/v1/auth/universal-auth/login"), &[], Some(&json!({ "clientId": id, "clientSecret": sec })), "Infisical").await?,
        None => {
            let identity = identity.context("an Infisical integration needs `identity_id` (OIDC), or a stored `<client id>:<client secret>` (universal auth)")?;
            call(reach, "POST", &format!("{site}/api/v1/auth/oidc-auth/login"), &[], Some(&json!({ "identityId": identity, "jwt": jwt.await? })), "Infisical").await?
        }
    };
    v.get("accessToken").and_then(Value::as_str).map(str::to_owned).context("Infisical gave no token")
}

/// A project's secret `name` in `environment` at `path`.
pub async fn infisical_read(reach: &Reach, site: &str, token: &str, project: &str, environment: &str, path: &str, name: &str) -> Result<String> {
    let q = format!("workspaceId={}&environment={}&secretPath={}", enc(project), enc(environment), enc(path));
    let auth = format!("Bearer {token}");
    let v = call(reach, "GET", &format!("{site}/api/v3/secrets/raw/{}?{q}", enc(name)), &[("authorization", &auth)], None, "Infisical").await?;
    v.pointer("/secret/secretValue").and_then(Value::as_str).map(str::to_owned).with_context(|| format!("Infisical has no value for {name}"))
}

// ------------------------------------------------------------------ Doppler

/// A Doppler token: the stored one, else a service account identity's by OIDC.
pub async fn doppler_login(reach: &Reach, api: &str, secret: Option<&str>, identity: Option<&str>, jwt: impl Future<Output = Result<String>>) -> Result<String> {
    if let Some(s) = secret {
        return Ok(s.to_owned());
    }
    let identity = identity.context("a Doppler integration needs a stored token, or a service account identity (`identity`) for OIDC")?;
    let v = call(reach, "POST", &format!("{api}/v3/auth/oidc"), &[], Some(&json!({ "identity": identity, "token": jwt.await? })), "Doppler").await?;
    v.get("token").and_then(Value::as_str).map(str::to_owned).context("Doppler gave no token")
}

/// A config's secret `name`, as Doppler computes it.
pub async fn doppler_read(reach: &Reach, api: &str, token: &str, project: &str, config: &str, name: &str) -> Result<String> {
    let auth = format!("Bearer {token}");
    let url = format!("{api}/v3/configs/config/secret?project={}&config={}&name={}", enc(project), enc(config), enc(name));
    let v = call(reach, "GET", &url, &[("authorization", &auth)], None, "Doppler").await?;
    v.pointer("/value/computed").or_else(|| v.pointer("/value/raw")).and_then(Value::as_str).map(str::to_owned).with_context(|| format!("Doppler has no value for {name}"))
}

// ------------------------------------------------------------------ 1Password Connect

/// A vault's item's field, each by name (or id), through a Connect server.
pub async fn onepassword_read(reach: &Reach, connect: &str, token: &str, vault: &str, item: &str, field: &str) -> Result<String> {
    let auth = format!("Bearer {token}");
    let h = [("authorization", auth.as_str())];
    let vaults = call(reach, "GET", &format!("{connect}/v1/vaults"), &h, None, "1Password Connect").await?;
    let vid = by_name(&vaults, vault, "name").with_context(|| format!("1Password has no vault {vault} this token reaches"))?;
    let items = call(reach, "GET", &format!("{connect}/v1/vaults/{vid}/items"), &h, None, "1Password Connect").await?;
    let iid = by_name(&items, item, "title").with_context(|| format!("1Password has no item {item} in {vault}"))?;
    let it = call(reach, "GET", &format!("{connect}/v1/vaults/{vid}/items/{iid}"), &h, None, "1Password Connect").await?;
    it.get("fields")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .find(|f| [f.get("label"), f.get("id")].iter().any(|x| x.and_then(|v| v.as_str()) == Some(field)))
        .and_then(|f| f.get("value").and_then(Value::as_str))
        .map(str::to_owned)
        .with_context(|| format!("{vault}/{item} has no field {field}"))
}

/// The id of the entry of `list` whose `key` (or id) is `name`.
fn by_name(list: &Value, name: &str, key: &str) -> Option<String> {
    list.as_array()?.iter().find(|x| x.get(key).and_then(Value::as_str) == Some(name) || x.get("id").and_then(Value::as_str) == Some(name)).and_then(|x| x.get("id").and_then(Value::as_str)).map(str::to_owned)
}

fn enc(s: &str) -> String {
    s.bytes().map(|b| if b.is_ascii_alphanumeric() || b"-_.~".contains(&b) { (b as char).to_string() } else { format!("%{b:02X}") }).collect()
}

// ------------------------------------------------------------------ for the broker and Verify

/// Kiln's ID token for workload `w`, for `audience`.
async fn id_token(hub: &Hub, w: &kiln_ci::oidc::Workload<'_>, audience: &str) -> Result<String> {
    let issuer = crate::ci::oidc::issuer(hub);
    let claims = kiln_ci::oidc::workload_claims(w, &issuer, audience, &crate::util::random_token(), crate::util::now_unix(), 300, None);
    crate::ci::oidc::mint(hub, &claims).await
}

/// A token for store `i`, for workload `w`.
pub async fn sign_in(hub: &Hub, i: &Integration, w: &kiln_ci::oidc::Workload<'_>) -> Result<String> {
    let reach = hub.cfg.ci.reach();
    let secret = super::token(hub, i).await?;
    let stored = secret.is_some();
    let audience = cfg(i, "audience").unwrap_or_else(|| i.kind.clone());
    let t = match i.kind.as_str() {
        "vault" => {
            let address = cfg(i, "address").context("a Vault integration needs its `address`")?;
            let role = cfg(i, "role").context("a Vault integration needs the JWT auth `role` kiln signs in as")?;
            let jwt = id_token(hub, w, &audience).await?;
            vault_login(&reach, &address, &cfg(i, "auth_path").unwrap_or_else(|| "jwt".into()), &role, &jwt, cfg(i, "namespace").as_deref()).await?
        }
        "infisical" => {
            let site = cfg(i, "site").unwrap_or_else(|| super::endpoint("infisical", || "https://app.infisical.com".into()));
            infisical_login(&reach, &site, secret.as_deref(), cfg(i, "identity_id").as_deref(), id_token(hub, w, &audience)).await?
        }
        "doppler" => {
            let api = super::endpoint("doppler", || "https://api.doppler.com".into());
            doppler_login(&reach, &api, secret.as_deref(), cfg(i, "identity").as_deref(), id_token(hub, w, &audience)).await?
        }
        "1password" => secret.clone().context("a 1Password integration needs its Connect server's token")?,
        other => bail!("`{other}` isn't a secret store"),
    };
    if !stored {
        crate::audit::exchange(hub, i, w, &i.kind).await;
    }
    Ok(t)
}

/// A secret's value from store `i`: `path` as its source names it.
pub async fn read(hub: &Hub, i: &Integration, w: &kiln_ci::oidc::Workload<'_>, path: &str) -> Result<String> {
    let reach = hub.cfg.ci.reach();
    let token = sign_in(hub, i, w).await?;
    match i.kind.as_str() {
        "vault" => {
            let address = cfg(i, "address").context("a Vault integration needs its `address`")?;
            let (p, key) = path.rsplit_once('#').context("a Vault secret names its key: vault:<path>#<key>")?;
            vault_read(&reach, &address, &token, p, key, cfg(i, "namespace").as_deref()).await
        }
        "infisical" => {
            let site = cfg(i, "site").unwrap_or_else(|| super::endpoint("infisical", || "https://app.infisical.com".into()));
            let project = cfg(i, "project_id").context("an Infisical integration needs its `project_id`")?;
            let (env, rest) = path.split_once('/').context("an Infisical secret is infisical:<environment>/<NAME>")?;
            let (dir, name) = rest.rsplit_once('/').map_or(("", rest), |(d, n)| (d, n));
            infisical_read(&reach, &site, &token, &project, env, &format!("/{dir}"), name).await
        }
        "doppler" => {
            let api = super::endpoint("doppler", || "https://api.doppler.com".into());
            let parts: Vec<&str> = path.splitn(3, '/').collect();
            let [project, config, name] = parts.as_slice() else { bail!("a Doppler secret is doppler:<project>/<config>/<NAME>") };
            doppler_read(&reach, &api, &token, project, config, name).await
        }
        "1password" => {
            let connect = cfg(i, "connect").context("a 1Password integration needs its Connect server (`connect`)")?;
            let parts: Vec<&str> = path.splitn(3, '/').collect();
            let [vault, item, field] = parts.as_slice() else { bail!("a 1Password secret is 1password:<vault>/<item>/<field>") };
            onepassword_read(&reach, &connect, &token, vault, item, field).await
        }
        other => bail!("`{other}` isn't a secret store"),
    }
}

/// Verify: a sign-in, as a leg of `w` would.
pub async fn verify(hub: &Hub, i: &Integration, w: &kiln_ci::oidc::Workload<'_>) -> Result<String, String> {
    match i.kind.as_str() {
        "1password" => {
            let connect = cfg(i, "connect").ok_or("a 1Password integration needs its Connect server (`connect`)")?;
            let token = super::token(hub, i).await.map_err(|e| format!("{e:#}"))?.ok_or("no Connect token stored")?;
            let v = call(&hub.cfg.ci.reach(), "GET", &format!("{connect}/v1/vaults"), &[("authorization", &format!("Bearer {token}"))], None, "1Password Connect").await.map_err(|e| format!("{e:#}"))?;
            Ok(format!("1Password Connect: {} vaults", v.as_array().map_or(0, Vec::len)))
        }
        _ => sign_in(hub, i, w).await.map(|_| format!("signed in to {}", i.kind)).map_err(|e| format!("{e:#}")),
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::*;
    use crate::http::server::{self as srv, Request, Response};

    struct Fake;

    impl srv::Handler for Fake {
        async fn handle(self: Arc<Self>, req: Request) -> Response {
            let (method, path, query) = (req.method().to_string(), req.uri().path().to_owned(), req.uri().query().unwrap_or("").to_owned());
            let header = |k: &str| srv::header_str(&req, &http::header::HeaderName::from_bytes(k.as_bytes()).unwrap_or(http::header::ACCEPT)).unwrap_or("").to_owned();
            let (vault, auth) = (header("x-vault-token"), header("authorization"));
            let body: Value = serde_json::from_slice(&srv::body_bytes(req.into_body(), 1 << 20).await.unwrap_or_default()).unwrap_or(Value::Null);
            let ok = |v: Value| srv::json(http::StatusCode::OK, &v);
            match (method.as_str(), path.as_str()) {
                ("POST", "/v1/auth/jwt/login") if body.get("role") == Some(&json!("ci")) && body.get("jwt") == Some(&json!("idt")) => ok(json!({ "auth": { "client_token": "hvs.tok", "policies": ["ci"] } })),
                ("GET", "/v1/secret/data/npm") if vault == "hvs.tok" => ok(json!({ "data": { "data": { "token": "npm_123" }, "metadata": {} } })),
                ("GET", "/v1/kv1/npm") if vault == "hvs.tok" => ok(json!({ "data": { "token": "v1_value" } })),
                ("POST", "/api/v1/auth/universal-auth/login") if body.get("clientId") == Some(&json!("cid")) => ok(json!({ "accessToken": "inf.tok" })),
                ("GET", "/api/v3/secrets/raw/DB_PASSWORD") if auth == "Bearer inf.tok" && query.contains("environment=prod") && query.contains("secretPath=%2Fbackend") => ok(json!({ "secret": { "secretValue": "s3cr3t" } })),
                ("POST", "/v3/auth/oidc") if body.get("identity") == Some(&json!("ident-1")) => ok(json!({ "token": "dp.oidc" })),
                ("GET", "/v3/configs/config/secret") if auth == "Bearer dp.oidc" && query.contains("name=STRIPE_KEY") => ok(json!({ "name": "STRIPE_KEY", "value": { "raw": "${OTHER}", "computed": "sk_live" } })),
                ("GET", "/v1/vaults") if auth == "Bearer op.tok" => ok(json!([{ "id": "v1", "name": "CI" }])),
                ("GET", "/v1/vaults/v1/items") if auth == "Bearer op.tok" => ok(json!([{ "id": "i1", "title": "Registry" }])),
                ("GET", "/v1/vaults/v1/items/i1") if auth == "Bearer op.tok" => ok(json!({ "fields": [{ "id": "username", "label": "username", "value": "bot" }, { "id": "f2", "label": "password", "value": "pa55" }] })),
                _ => srv::json(http::StatusCode::FORBIDDEN, &json!({ "errors": [format!("denied {method} {path}")] })),
            }
        }
    }

    /// W09-R08: each store's sign-in and read, against a fake of all four.
    #[test]
    fn w09_r08_secret_stores() -> anyhow::Result<()> {
        crate::rt::block_on(async {
            let tcp = hyper_rt::tcp::TcpListener::bind(std::net::SocketAddr::from(([127, 0, 0, 1], 0)), 16).map_err(|e| anyhow::anyhow!("{e}"))?;
            let addr = tcp.local_addr().map_err(|e| anyhow::anyhow!("{e}"))?;
            crate::rt::spawn(async move {
                let fake = Arc::new(Fake);
                while let Ok(s) = tcp.accept().await {
                    let (h, r) = (Arc::clone(&fake), s.peer_addr().ok());
                    crate::rt::spawn(srv::serve_connection(crate::http::io::RtIo::new(s), h, r));
                }
            });
            let base = format!("http://{addr}");
            let reach = Reach { allow: vec!["127.0.0.1".into()] };
            let jwt = || async { Ok("idt".to_owned()) };

            let t = vault_login(&reach, &base, "jwt", "ci", "idt", None).await?;
            assert_eq!(vault_read(&reach, &base, &t, "secret/data/npm", "token", None).await?, "npm_123", "KV v2");
            assert_eq!(vault_read(&reach, &base, &t, "kv1/npm", "token", None).await?, "v1_value", "KV v1");
            assert!(vault_read(&reach, &base, &t, "secret/data/npm", "nope", None).await.is_err(), "no such key");
            assert!(vault_login(&reach, &base, "jwt", "other", "idt", None).await.is_err(), "another role is refused");

            let t = infisical_login(&reach, &base, Some("cid:shh"), None, jwt()).await?;
            assert_eq!(infisical_read(&reach, &base, &t, "proj", "prod", "/backend", "DB_PASSWORD").await?, "s3cr3t");
            assert!(infisical_login(&reach, &base, None, None, jwt()).await.is_err(), "neither way to sign in");

            let t = doppler_login(&reach, &base, None, Some("ident-1"), jwt()).await?;
            assert_eq!(doppler_read(&reach, &base, &t, "web", "prd", "STRIPE_KEY").await?, "sk_live", "the computed value");
            assert_eq!(doppler_login(&reach, &base, Some("dp.st.x"), None, jwt()).await?, "dp.st.x", "a service token as it is");

            assert_eq!(onepassword_read(&reach, &base, "op.tok", "CI", "Registry", "password").await?, "pa55");
            assert!(onepassword_read(&reach, &base, "op.tok", "CI", "Nope", "password").await.is_err());

            // Off the server's reach, nothing is asked.
            let closed = Reach { allow: vec![] };
            assert!(vault_login(&closed, &base, "jwt", "ci", "idt", None).await.is_err());
            Ok(())
        })
    }
}
