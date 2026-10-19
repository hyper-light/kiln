//! kiln as Docker's credential helper in a leg (docs/ci.md W12-R03): `docker-credential-kiln` (the
//! runner's own binary, by that name, beside `kiln` on the leg's `PATH`) answers Docker's `get` for
//! each registry the leg's `oci` integrations name, and for kiln's own, with credentials made when
//! Docker asks, short-lived where the registry federates:
//! - **ECR** (`federation = "aws"`): the leg's ID token for an AWS role, then ECR's authorization
//!   token;
//! - **Artifact Registry** (`"gcp"`): Google's federated token (a service account's, impersonated)
//!   as `oauth2accesstoken`;
//! - **ACR** (`"azure"`): Entra's token for the client, exchanged at the registry for its refresh
//!   token;
//! - **the rest** (Docker Hub, GHCR, GitLab, Harbor robots, …): the integration's stored token;
//! - **kiln's own registry**: the job's token, which ends with the leg.
//!
//! Nothing is written down: what Docker gets lives in its process, and an ID token can't be had
//! after the leg.

use std::io::{Read as _, Write as _};

use anyhow::{Context as _, anyhow};
use base64::Engine as _;
use kiln_ci::wire::Integration;
use serde_json::{Value, json};

/// The registry host a credential helper is asked about (`https://index.docker.io/v1/`,
/// `ghcr.io`, `https://acme.azurecr.io`): its host, as integrations name it.
pub fn host_of(server: &str) -> String {
    let s = server.trim();
    let s = s.split_once("://").map_or(s, |(_, r)| r);
    let host = s.split('/').next().unwrap_or(s).to_ascii_lowercase();
    if host == "index.docker.io" || host == "registry-1.docker.io" { "docker.io".to_owned() } else { host }
}

fn cfg<'a>(i: &'a Integration, k: &str) -> Option<&'a str> {
    i.config.get(k).and_then(Value::as_str).map(str::trim).filter(|s| !s.is_empty())
}

/// The integration of the leg's that names `host`.
fn integration_for<'a>(list: &'a [Integration], host: &str) -> Option<&'a Integration> {
    list.iter().find(|i| i.kind == "oci" && cfg(i, "registry").is_some_and(|r| host_of(r) == host))
}

/// An ECR registry's region: from its host (`<account>.dkr.ecr.<region>.amazonaws.com`).
pub fn ecr_region(host: &str) -> Option<String> {
    let rest = host.split_once(".dkr.ecr.")?.1;
    rest.split('.').next().filter(|r| !r.is_empty()).map(str::to_owned)
}

/// A username and secret for `host`, for the leg (its integrations, kiln's own registry).
pub async fn credentials_for(host: &str, list: &[Integration]) -> anyhow::Result<Option<(String, String)>> {
    use crate::ci::integrations::{aws, azure, gcp};
    // kiln's own registry: the job's token.
    if let (Ok(own), Ok(token)) = (std::env::var("KILN_REGISTRY_HOST"), std::env::var("KILN_TOKEN"))
        && host_of(&own) == host
        && !token.is_empty()
    {
        return Ok(Some(("kiln".to_owned(), token)));
    }
    let Some(i) = integration_for(list, host) else { return Ok(None) };
    let http = crate::http::client::Client::public()?;
    let token_for = |aud: String| {
        let http = &http;
        async move {
            let url = std::env::var("ACTIONS_ID_TOKEN_REQUEST_URL").map_err(|_| anyhow!("this leg gets no ID tokens: its job needs `permissions: id-token: write`"))?;
            let request = std::env::var("ACTIONS_ID_TOKEN_REQUEST_TOKEN").unwrap_or_default();
            super::auth::id_token(http, &url, &request, &aud).await
        }
    };
    match cfg(i, "federation") {
        Some("aws") => {
            let role = cfg(i, "role_arn").ok_or_else(|| anyhow!("`{}` has no role_arn", i.name))?;
            let region = cfg(i, "region").map(str::to_owned).or_else(|| ecr_region(host)).ok_or_else(|| anyhow!("`{}`: which region is {host} in? (`region`)", i.name))?;
            let jwt = token_for(cfg(i, "audience").unwrap_or("sts.amazonaws.com").to_owned()).await?;
            let session = aws::session_name(&format!("kiln-{}", std::env::var("GITHUB_REPOSITORY").unwrap_or_default()));
            let creds = aws::assume_role_with_web_identity(&aws::sts_endpoint(Some(&region)), role, &session, &jwt, 3600).await?;
            let client = aws::Aws { creds, region };
            let v = client.json("ecr", "AmazonEC2ContainerRegistry_V20150921.GetAuthorizationToken", &json!({})).await?;
            let encoded = v.pointer("/authorizationData/0/authorizationToken").and_then(Value::as_str).ok_or_else(|| anyhow!("ECR's answer has no authorization token"))?;
            let pair = String::from_utf8(base64::engine::general_purpose::STANDARD.decode(encoded)?)?;
            let (user, secret) = pair.split_once(':').ok_or_else(|| anyhow!("ECR's token isn't user:password"))?;
            Ok(Some((user.to_owned(), secret.to_owned())))
        }
        Some("gcp") => {
            let provider = cfg(i, "provider").ok_or_else(|| anyhow!("`{}` has no provider", i.name))?;
            let provider = provider.trim_start_matches("https:").trim_start_matches("//iam.googleapis.com/").trim_start_matches('/').to_owned();
            let jwt = token_for(cfg(i, "audience").map_or_else(|| format!("//iam.googleapis.com/{provider}"), str::to_owned)).await?;
            let federated = gcp::exchange(&provider, &jwt).await?;
            let t = match cfg(i, "service_account") {
                Some(sa) => gcp::impersonate(sa, &federated.access_token).await?,
                None => federated,
            };
            Ok(Some(("oauth2accesstoken".to_owned(), t.access_token)))
        }
        Some("azure") => {
            let (tenant, client) = (cfg(i, "tenant_id").ok_or_else(|| anyhow!("`{}` has no tenant_id", i.name))?, cfg(i, "client_id").ok_or_else(|| anyhow!("`{}` has no client_id", i.name))?);
            let jwt = token_for(azure::AUDIENCE.to_owned()).await?;
            let aad = azure::exchange(tenant, client, azure::MANAGEMENT, &jwt).await?;
            // ACR takes Entra's token for one of its own (docs: the registry's /oauth2/exchange).
            let form = format!("grant_type=access_token&service={}&tenant={}&access_token={}", crate::ci::cli::enc(host), crate::ci::cli::enc(tenant), crate::ci::cli::enc(&aad.access_token));
            let r = http.post(&format!("https://{host}/oauth2/exchange")).header("Content-Type", "application/x-www-form-urlencoded").body(form).send().await.context("ACR's token exchange")?;
            anyhow::ensure!(r.is_success(), "ACR refused the exchange ({})", r.status());
            let v: Value = r.json(1 << 20).await?;
            let refresh = v.get("refresh_token").and_then(Value::as_str).ok_or_else(|| anyhow!("ACR's answer has no refresh token"))?;
            Ok(Some(("00000000-0000-0000-0000-000000000000".to_owned(), refresh.to_owned())))
        }
        Some(other) => Err(anyhow!("`{}`: federation `{other}`: aws, gcp or azure (or none, for its token)", i.name)),
        None => {
            let t = i.token.clone().ok_or_else(|| anyhow!("`{}` has no token, and no federation", i.name))?;
            Ok(Some((cfg(i, "username").unwrap_or("kiln").to_owned(), t)))
        }
    }
}

/// Docker's credential helper protocol: `get` (the server on stdin, the credentials as JSON on
/// stdout; `false` when it has none for that registry, which exits 1 as Docker expects), `store`,
/// `erase` and `list` (nothing is kept, so there's nothing to change or list).
pub async fn helper(action: &str) -> anyhow::Result<bool> {
    let mut input = String::new();
    std::io::stdin().read_to_string(&mut input)?;
    let (said, found) = match action {
        "get" => {
            let list: Vec<Integration> = match std::env::var_os("KILN_INTEGRATIONS") {
                Some(p) => serde_json::from_slice(&std::fs::read(&p)?)?,
                None => Vec::new(),
            };
            match credentials_for(&host_of(&input), &list).await? {
                Some((user, secret)) => (json!({ "ServerURL": input.trim(), "Username": user, "Secret": secret }).to_string(), true),
                // What Docker takes as "none here".
                None => ("credentials not found in native keychain".to_owned(), false),
            }
        }
        "list" => ("{}".to_owned(), true),
        "store" | "erase" => (String::new(), true),
        other => anyhow::bail!("docker-credential-kiln: `{other}`: get, store, erase or list"),
    };
    if !said.is_empty() {
        writeln!(std::io::stdout().lock(), "{said}")?;
    }
    Ok(found)
}

/// The Docker config a leg gets: the runner's own (`base`: what `docker login` left it, its
/// credential store), with kiln asked for every registry the leg's `oci` integrations name and for
/// kiln's own (`own`, the server's host).
pub fn docker_config(list: &[Integration], own: Option<&str>, base: Option<Value>) -> Value {
    let mut conf = base.filter(Value::is_object).unwrap_or_else(|| json!({}));
    let mut helpers = conf.get("credHelpers").and_then(Value::as_object).cloned().unwrap_or_default();
    for i in list.iter().filter(|i| i.kind == "oci") {
        if let Some(r) = cfg(i, "registry") {
            let host = host_of(r);
            let key = if host == "docker.io" { "https://index.docker.io/v1/".to_owned() } else { host };
            helpers.insert(key, json!("kiln"));
        }
    }
    if let Some(o) = own {
        helpers.insert(host_of(o), json!("kiln"));
    }
    if let Some(o) = conf.as_object_mut() {
        o.insert("credHelpers".into(), Value::Object(helpers));
    }
    conf
}

/// The runner's own Docker config (`$DOCKER_CONFIG/config.json`, else `~/.docker/config.json`).
pub fn runner_config() -> Option<Value> {
    let dir = std::env::var_os("DOCKER_CONFIG").map(std::path::PathBuf::from).or_else(|| dirs_home().map(|h| h.join(".docker")))?;
    serde_json::from_slice(&std::fs::read(dir.join("config.json")).ok()?).ok()
}

fn dirs_home() -> Option<std::path::PathBuf> {
    std::env::var_os("HOME").or_else(|| std::env::var_os("USERPROFILE")).map(std::path::PathBuf::from)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn w12_r03_hosts_and_configs() {
        assert_eq!(host_of("https://index.docker.io/v1/"), "docker.io");
        assert_eq!(host_of("https://ACME.azurecr.io"), "acme.azurecr.io");
        assert_eq!(host_of("ghcr.io"), "ghcr.io");
        assert_eq!(ecr_region("123456789012.dkr.ecr.eu-west-1.amazonaws.com").as_deref(), Some("eu-west-1"));
        assert_eq!(ecr_region("ghcr.io"), None);
        let i = |name: &str, registry: &str| Integration { name: name.into(), kind: "oci".into(), mode: "oidc".into(), config: json!({ "registry": registry }), token: None, job: true };
        let list = [i("ecr", "123456789012.dkr.ecr.eu-west-1.amazonaws.com"), i("hub", "docker.io")];
        assert_eq!(integration_for(&list, "docker.io").map(|i| i.name.as_str()), Some("hub"));
        let base = json!({ "auths": { "registry.corp": { "auth": "eA==" } }, "credsStore": "desktop", "credHelpers": { "gcr.io": "gcloud" } });
        let c = docker_config(&list, Some("https://kiln.example:7070"), Some(base));
        assert_eq!(c.pointer("/credHelpers/123456789012.dkr.ecr.eu-west-1.amazonaws.com"), Some(&json!("kiln")));
        assert_eq!(c.pointer("/credHelpers/https:~1~1index.docker.io~1v1~1"), Some(&json!("kiln")));
        assert_eq!(c.pointer("/credHelpers/kiln.example:7070"), Some(&json!("kiln")));
        assert_eq!((c.pointer("/credHelpers/gcr.io"), c.get("credsStore"), c.pointer("/auths/registry.corp/auth")), (Some(&json!("gcloud")), Some(&json!("desktop")), Some(&json!("eA=="))), "the runner's own logins stay");
    }
}
