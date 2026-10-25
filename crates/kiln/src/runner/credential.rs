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
//! - **Quay** (`"quay"`): a robot's federated token; **JFrog** (`"jfrog"`): an Artifactory access
//!   token for its OIDC provider; **Cloudsmith** (`"cloudsmith"`): a service's token;
//!   **Chainguard** (`"chainguard"`): an assumable identity's token;
//! - **Alibaba's Container Registry** (`"alibaba"`): a RAM role's credentials for the leg's ID
//!   token, then the registry's temporary password (an Enterprise Edition instance's, with
//!   `instance_id`, or the Personal Edition's);
//! - **Oracle's Container Registry** (`"oracle"`): an OCI identity domain's session token for the
//!   leg's ID token, bound to a key made for it, then the registry's token for a request that key
//!   signs (an auth token, with no federation, is the stored token, its `username`
//!   `<namespace>/<user>`);
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
        Some("quay") => {
            // Quay's robot federation: the robot's name, the ID token as its password.
            let robot = cfg(i, "robot").ok_or_else(|| anyhow!("`{}` has no robot (`org+name`)", i.name))?;
            let jwt = token_for(cfg(i, "audience").unwrap_or(host).to_owned()).await?;
            let basic = base64::engine::general_purpose::STANDARD.encode(format!("{robot}:{jwt}"));
            let r = http.get(&format!("https://{host}/oauth2/federation/robot/token")).header("Authorization", &format!("Basic {basic}")).send().await.context("Quay's robot federation")?;
            anyhow::ensure!(r.is_success(), "Quay refused the robot's federation ({})", r.status());
            let v: Value = r.json(1 << 20).await?;
            let token = v.get("token").and_then(Value::as_str).ok_or_else(|| anyhow!("Quay's answer has no token"))?;
            Ok(Some((robot.to_owned(), token.to_owned())))
        }
        Some("jfrog") => {
            // Artifactory's OIDC integration: a token exchange at its access service.
            let provider = cfg(i, "provider").ok_or_else(|| anyhow!("`{}` has no provider (the OIDC integration's name in JFrog)", i.name))?;
            let jwt = token_for(cfg(i, "audience").unwrap_or("jfrog-github").to_owned()).await?;
            let body = json!({ "grant_type": "urn:ietf:params:oauth:grant-type:token-exchange", "subject_token_type": "urn:ietf:params:oauth:token-type:id_token", "subject_token": jwt, "provider_name": provider });
            let base = cfg(i, "url").map_or_else(|| format!("https://{host}"), |u| u.trim_end_matches('/').to_owned());
            let r = http.post(&format!("{base}/access/api/v1/oidc/token")).json(&body).send().await.context("JFrog's token exchange")?;
            anyhow::ensure!(r.is_success(), "JFrog refused the exchange ({})", r.status());
            let v: Value = r.json(1 << 20).await?;
            let token = v.get("access_token").and_then(Value::as_str).ok_or_else(|| anyhow!("JFrog's answer has no access token"))?;
            let user = v.get("username").and_then(Value::as_str).or_else(|| cfg(i, "username")).unwrap_or("kiln");
            Ok(Some((user.to_owned(), token.to_owned())))
        }
        Some("cloudsmith") => {
            let (org, service) = (cfg(i, "org").ok_or_else(|| anyhow!("`{}` has no org", i.name))?, cfg(i, "service").ok_or_else(|| anyhow!("`{}` has no service (its slug)", i.name))?);
            // The audience is the one the Cloudsmith OIDC provider was set up with.
            let jwt = token_for(cfg(i, "audience").ok_or_else(|| anyhow!("`{}` has no audience (the one its Cloudsmith OIDC provider expects)", i.name))?.to_owned()).await?;
            let r = http.post(&format!("https://api.cloudsmith.io/openid/{org}/")).json(&json!({ "oidc_token": jwt, "service_slug": service })).send().await.context("Cloudsmith's OIDC exchange")?;
            anyhow::ensure!(r.is_success(), "Cloudsmith refused the exchange ({})", r.status());
            let v: Value = r.json(1 << 20).await?;
            let token = v.get("token").and_then(Value::as_str).ok_or_else(|| anyhow!("Cloudsmith's answer has no token"))?;
            Ok(Some((service.to_owned(), token.to_owned())))
        }
        Some("chainguard") => {
            let identity = cfg(i, "identity").ok_or_else(|| anyhow!("`{}` has no identity (the assumable identity's id)", i.name))?;
            let jwt = token_for(cfg(i, "audience").unwrap_or("https://issuer.enforce.dev").to_owned()).await?;
            let url = format!("https://issuer.enforce.dev/sts/exchange?aud={}&identity={}", crate::ci::cli::enc(host), crate::ci::cli::enc(identity));
            let r = http.post(&url).bearer(&jwt).send().await.context("Chainguard's token exchange")?;
            anyhow::ensure!(r.is_success(), "Chainguard refused the exchange ({})", r.status());
            let v: Value = r.json(1 << 20).await?;
            let token = v.get("token").and_then(Value::as_str).ok_or_else(|| anyhow!("Chainguard's answer has no token"))?;
            Ok(Some(("_token".to_owned(), token.to_owned())))
        }
        Some("alibaba") => {
            use crate::ci::integrations::alibaba;
            let (role, provider) = (
                cfg(i, "role_arn").ok_or_else(|| anyhow!("`{}` has no role_arn (`acs:ram::<account>:role/<name>`)", i.name))?,
                cfg(i, "oidc_provider_arn").ok_or_else(|| anyhow!("`{}` has no oidc_provider_arn", i.name))?,
            );
            let region = cfg(i, "region").map(str::to_owned).or_else(|| alibaba::acr_region(host)).ok_or_else(|| anyhow!("`{}`: which region is {host} in? (`region`)", i.name))?;
            let jwt = token_for(cfg(i, "audience").unwrap_or("sts.aliyuncs.com").to_owned()).await?;
            let session = aws::session_name(&format!("kiln-{}", std::env::var("GITHUB_REPOSITORY").unwrap_or_default()));
            let creds = alibaba::assume_role_with_oidc(&http, Some(&region), provider, role, &session, &jwt, 3600).await?;
            Ok(Some(alibaba::acr_login(&http, &region, cfg(i, "instance_id"), &creds).await?))
        }
        Some("oracle") => {
            use crate::ci::integrations::oracle;
            let (domain, client) =
                (cfg(i, "domain").ok_or_else(|| anyhow!("`{}` has no domain (its OCI identity domain's URL)", i.name))?, cfg(i, "client_id").ok_or_else(|| anyhow!("`{}` has no client_id", i.name))?);
            let secret = i.token.clone().ok_or_else(|| anyhow!("`{}` has no client secret (its token)", i.name))?;
            let jwt = token_for(cfg(i, "audience").unwrap_or(domain).to_owned()).await?;
            let key = oracle::SessionKey::new()?;
            let session = oracle::exchange(&http, domain, client, &secret, &jwt, &key.public()?).await?;
            Ok(Some((oracle::BEARER_USER.to_owned(), oracle::registry_token(&http, host, &session, &key).await?)))
        }
        Some(other) => Err(anyhow!("`{}`: federation `{other}`: aws, gcp, azure, quay, jfrog, cloudsmith, chainguard, alibaba or oracle (or none, for its token)", i.name)),
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

/// What a leg's tools need to go through kiln's package proxies (W12-R04): their variables, and
/// the files some read their settings from (written in `dir`; `seen` is how a step sees a path),
/// with the job's `token`.
pub fn proxy_env(proxies: &[(String, String)], token: &str, dir: &std::path::Path, seen: &dyn Fn(&std::path::Path) -> String) -> anyhow::Result<std::collections::BTreeMap<String, String>> {
    let mut env = std::collections::BTreeMap::new();
    std::fs::create_dir_all(dir)?;
    let (mut npmrc, mut netrc) = (String::new(), String::new());
    for (eco, url) in proxies {
        let url = url.trim_end_matches('/');
        let (scheme, rest) = url.split_once("://").unwrap_or(("https", url));
        let host = rest.split('/').next().unwrap_or(rest);
        let bare = host.rsplit_once(':').map_or(host, |(h, _)| h);
        let with_creds = format!("{scheme}://kiln:{token}@{rest}");
        match crate::ci::registry::proxy::Ecosystem::parse(eco) {
            Some(crate::ci::registry::proxy::Ecosystem::Npm) => npmrc.push_str(&format!("registry={url}/\n//{rest}/:_authToken={token}\n")),
            Some(crate::ci::registry::proxy::Ecosystem::Jsr) => npmrc.push_str(&format!("@jsr:registry={url}/\n//{rest}/:_authToken={token}\n")),
            Some(crate::ci::registry::proxy::Ecosystem::Pypi) => {
                let index = format!("{with_creds}/simple/");
                env.insert("PIP_INDEX_URL".to_owned(), index.clone());
                env.insert("UV_DEFAULT_INDEX".to_owned(), index.clone());
                env.insert("UV_INDEX_URL".to_owned(), index);
                if scheme == "http" {
                    env.insert("PIP_TRUSTED_HOST".to_owned(), bare.to_owned());
                    env.insert("UV_INSECURE_HOST".to_owned(), host.to_owned());
                }
            }
            Some(crate::ci::registry::proxy::Ecosystem::Go) => {
                env.insert("GOPROXY".to_owned(), url.to_owned());
                // Go signs in to a proxy from its netrc, by the host's name.
                netrc.push_str(&format!("machine {bare} login kiln password {token}\n"));
            }
            Some(crate::ci::registry::proxy::Ecosystem::Maven) => {
                let f = dir.join("maven-settings.xml");
                super::auth::write_private(&f, format!("<settings><mirrors><mirror><id>kiln</id><mirrorOf>*</mirrorOf><url>{url}</url></mirror></mirrors><servers><server><id>kiln</id><username>kiln</username><password>{token}</password></server></servers></settings>\n").as_bytes())?;
                env.insert("MAVEN_ARGS".to_owned(), format!("-s {}", seen(&f)));
            }
            Some(crate::ci::registry::proxy::Ecosystem::Rubygems) => {
                let f = dir.join("gemrc");
                super::auth::write_private(&f, format!("---\n:sources:\n- {with_creds}/\n").as_bytes())?;
                env.insert("GEMRC".to_owned(), seen(&f));
                env.insert("BUNDLE_MIRROR__HTTPS://RUBYGEMS__ORG/".to_owned(), format!("{with_creds}/"));
            }
            Some(crate::ci::registry::proxy::Ecosystem::Conda) => {
                env.insert("CONDA_CHANNEL_ALIAS".to_owned(), url.to_owned());
            }
            // Cargo's source replacement, NuGet's and the others' settings live in files a project
            // names itself (docs/ci.md W12-R04).
            _ => {}
        }
    }
    if !npmrc.is_empty() {
        let f = dir.join("npmrc");
        super::auth::write_private(&f, npmrc.as_bytes())?;
        env.insert("NPM_CONFIG_USERCONFIG".to_owned(), seen(&f));
    }
    if !netrc.is_empty() {
        let f = dir.join("netrc");
        super::auth::write_private(&f, netrc.as_bytes())?;
        env.insert("NETRC".to_owned(), seen(&f));
    }
    Ok(env)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn w12_r04_proxy_settings() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let proxies = [("npm", "http://k:7/_kiln/proxy/npm"), ("pypi", "http://k:7/_kiln/proxy/pypi"), ("go", "http://k:7/_kiln/proxy/go"), ("maven", "http://k:7/_kiln/proxy/maven"), ("rubygems", "http://k:7/_kiln/proxy/rubygems")]
            .map(|(e, u)| (e.to_owned(), u.to_owned()));
        let env = proxy_env(&proxies, "T0K", dir.path(), &|p| p.to_string_lossy().into_owned())?;
        let read = |k: &str| env.get(k).map(|f| std::fs::read_to_string(f).unwrap_or_default()).unwrap_or_default();
        assert_eq!(read("NPM_CONFIG_USERCONFIG"), "registry=http://k:7/_kiln/proxy/npm/\n//k:7/_kiln/proxy/npm/:_authToken=T0K\n");
        assert_eq!(env.get("PIP_INDEX_URL").map(String::as_str), Some("http://kiln:T0K@k:7/_kiln/proxy/pypi/simple/"));
        assert_eq!((env.get("PIP_TRUSTED_HOST").map(String::as_str), env.get("UV_INSECURE_HOST").map(String::as_str)), (Some("k"), Some("k:7")));
        assert_eq!(read("NETRC"), "machine k login kiln password T0K\n");
        assert_eq!(env.get("GOPROXY").map(String::as_str), Some("http://k:7/_kiln/proxy/go"));
        assert!(env.get("MAVEN_ARGS").is_some_and(|a| a.starts_with("-s ")));
        assert!(read("GEMRC").contains("- http://kiln:T0K@k:7/_kiln/proxy/rubygems/"));
        Ok(())
    }

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
