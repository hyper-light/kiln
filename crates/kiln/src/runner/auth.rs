//! Integrations in legs (docs/ci.md W11-R03, R04): each as its provider's own tools expect it.
//!
//! - **Federating kinds** get an ID token for the provider's audience in a file, which the runner
//!   rewrites every four minutes while the leg runs (tokens live five):
//!   - `aws`: `AWS_ROLE_ARN`, `AWS_WEB_IDENTITY_TOKEN_FILE`, `AWS_ROLE_SESSION_NAME` and the
//!     region, which every AWS SDK and the CLI exchange by themselves;
//!   - `gcp`: an `external_account` credential file (`GOOGLE_APPLICATION_CREDENTIALS`, gcloud's
//!     `CLOUDSDK_AUTH_CREDENTIAL_FILE_OVERRIDE`) and the project;
//!   - `azure`: `AZURE_CLIENT_ID`, `AZURE_TENANT_ID` and `AZURE_FEDERATED_TOKEN_FILE` (Azure
//!     Identity, `az login --federated-token`), and Terraform's `ARM_*`.
//! - **Token kinds** get their stored token as the variable their tools read
//!   (`CLOUDFLARE_API_TOKEN`, `VERCEL_TOKEN`, …), masked.
//! - `kiln auth <integration>` exchanges for short-lived credentials itself, for tools that take
//!   those but no federation file (`--print-env`, `--exec CMD…`).

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Weak};
use std::time::Duration;

use kiln_ci::wire::Integration;
use parking_lot::Mutex;
use serde_json::{Value, json};

/// What an integration gives a leg's steps.
#[derive(Debug, Default, Clone)]
pub struct Applied {
    pub env: BTreeMap<String, String>,
    /// ID token files to keep fresh, with their audiences.
    pub tokens: Vec<(PathBuf, String)>,
    /// Values to mask.
    pub masks: Vec<String>,
}

fn cfg<'a>(i: &'a Integration, k: &str) -> Option<&'a str> {
    i.config.get(k).and_then(Value::as_str).map(str::trim).filter(|s| !s.is_empty())
}

fn gcp_provider(p: &str) -> &str {
    p.trim_start_matches("https:").trim_start_matches("//iam.googleapis.com/").trim_start_matches('/')
}

/// The audience a federating kind's ID token is for.
pub fn audience(i: &Integration) -> Option<String> {
    match i.kind.as_str() {
        "aws" => Some(cfg(i, "audience").unwrap_or("sts.amazonaws.com").to_owned()),
        "gcp" => cfg(i, "provider").map(|p| format!("https://iam.googleapis.com/{}", gcp_provider(p))),
        "azure" => Some("api://AzureADTokenExchange".to_owned()),
        _ => None,
    }
}

/// The variable a token kind's tools read its token from.
const TOKEN_ENV: &[(&str, &str)] = &[
    ("cloudflare", "CLOUDFLARE_API_TOKEN"),
    ("vercel", "VERCEL_TOKEN"),
    ("discord", "DISCORD_WEBHOOK_URL"),
    ("teams", "TEAMS_WEBHOOK_URL"),
    ("telegram", "TELEGRAM_BOT_TOKEN"),
    ("pagerduty", "PAGERDUTY_ROUTING_KEY"),
    ("opsgenie", "OPSGENIE_API_KEY"),
    ("railway", "RAILWAY_TOKEN"),
    ("fly", "FLY_API_TOKEN"),
    ("netlify", "NETLIFY_AUTH_TOKEN"),
    ("render", "RENDER_API_KEY"),
    ("heroku", "HEROKU_API_KEY"),
    ("digitalocean", "DIGITALOCEAN_ACCESS_TOKEN"),
    ("nomad", "NOMAD_TOKEN"),
    ("terraform", "TF_API_TOKEN"),
    ("pulumi", "PULUMI_ACCESS_TOKEN"),
    ("webhook", "KILN_WEBHOOK_SECRET"),
];

/// A name made safe for a file or a variable.
fn safe(s: &str) -> String {
    s.chars().map(|c| if c.is_ascii_alphanumeric() { c } else { '_' }).collect()
}

/// AWS's rules for a role session's name: 2 to 64 of `[\w+=,.@-]`, the rest as dashes.
fn session_name(s: &str) -> String {
    let mut n = String::new();
    for c in s.chars() {
        let c = if c.is_ascii_alphanumeric() || "+=,.@_".contains(c) { c } else { '-' };
        if !(c == '-' && (n.is_empty() || n.ends_with('-'))) {
            n.push(c);
        }
    }
    let mut n: String = n.trim_end_matches('-').chars().take(64).collect();
    while n.len() < 2 {
        n.push('x');
    }
    n
}

/// What `i` gives steps. Files go under `dir`; `seen` is how a step sees a path (inside a job
/// container, its path there). The ID token files are named here and filled by the caller.
pub fn apply(i: &Integration, dir: &Path, session: &str, seen: &dyn Fn(&Path) -> String) -> anyhow::Result<Applied> {
    let mut a = Applied::default();
    let token_file = dir.join(format!("{}.jwt", safe(&i.name)));
    let mut env: Vec<(String, String)> = Vec::new();
    let mut set = |k: &str, v: &str| env.push((k.to_owned(), v.to_owned()));
    match i.kind.as_str() {
        "aws" => {
            let role = cfg(i, "role_arn").ok_or_else(|| anyhow::anyhow!("`{}` has no role_arn", i.name))?;
            set("AWS_ROLE_ARN", role);
            set("AWS_WEB_IDENTITY_TOKEN_FILE", &token_file.to_string_lossy());
            set("AWS_ROLE_SESSION_NAME", &session_name(session));
            if let Some(r) = cfg(i, "region") {
                set("AWS_REGION", r);
                set("AWS_DEFAULT_REGION", r);
            }
            a.tokens.push((token_file, audience(i).unwrap_or_default()));
        }
        "gcp" => {
            let provider = gcp_provider(cfg(i, "provider").ok_or_else(|| anyhow::anyhow!("`{}` has no provider", i.name))?);
            let mut creds = json!({
                "type": "external_account",
                "audience": format!("//iam.googleapis.com/{provider}"),
                "subject_token_type": "urn:ietf:params:oauth:token-type:jwt",
                "token_url": "https://sts.googleapis.com/v1/token",
                "credential_source": { "file": seen(&token_file) },
            });
            if let (Some(sa), Some(o)) = (cfg(i, "service_account"), creds.as_object_mut()) {
                o.insert("service_account_impersonation_url".into(), json!(format!("https://iamcredentials.googleapis.com/v1/projects/-/serviceAccounts/{sa}:generateAccessToken")));
            }
            let file = dir.join(format!("{}-gcp.json", safe(&i.name)));
            write_private(&file, serde_json::to_string_pretty(&creds)?.as_bytes())?;
            let f = file.to_string_lossy();
            for k in ["GOOGLE_APPLICATION_CREDENTIALS", "CLOUDSDK_AUTH_CREDENTIAL_FILE_OVERRIDE", "GOOGLE_GHA_CREDS_PATH"] {
                set(k, &f);
            }
            if let Some(p) = cfg(i, "project") {
                for k in ["GOOGLE_CLOUD_PROJECT", "CLOUDSDK_CORE_PROJECT", "CLOUDSDK_PROJECT", "GCLOUD_PROJECT", "GCP_PROJECT"] {
                    set(k, p);
                }
            }
            a.tokens.push((token_file, audience(i).unwrap_or_default()));
        }
        "azure" => {
            let (tenant, client) = (cfg(i, "tenant_id").ok_or_else(|| anyhow::anyhow!("`{}` has no tenant_id", i.name))?, cfg(i, "client_id").ok_or_else(|| anyhow::anyhow!("`{}` has no client_id", i.name))?);
            let f = token_file.to_string_lossy();
            for (k, v) in [("AZURE_CLIENT_ID", client), ("AZURE_TENANT_ID", tenant), ("AZURE_FEDERATED_TOKEN_FILE", &f), ("ARM_CLIENT_ID", client), ("ARM_TENANT_ID", tenant), ("ARM_USE_OIDC", "true"), ("ARM_OIDC_TOKEN_FILE_PATH", &f)] {
                set(k, v);
            }
            if let Some(s) = cfg(i, "subscription_id") {
                set("AZURE_SUBSCRIPTION_ID", s);
                set("ARM_SUBSCRIPTION_ID", s);
            }
            a.tokens.push((token_file, audience(i).unwrap_or_default()));
        }
        "oci" => {
            let registry = cfg(i, "registry").unwrap_or("docker.io");
            set("KILN_REGISTRY", registry);
            if let Some(t) = i.token.as_deref() {
                // As `docker login` leaves it: a config with the registry's credentials.
                let user = cfg(i, "username").unwrap_or("kiln");
                let host = if registry == "docker.io" { "https://index.docker.io/v1/" } else { registry };
                let auth = base64::Engine::encode(&base64::engine::general_purpose::STANDARD, format!("{user}:{t}"));
                let conf = dir.join(format!("{}-docker", safe(&i.name)));
                std::fs::create_dir_all(&conf)?;
                write_private(&conf.join("config.json"), serde_json::to_string(&json!({ "auths": { host: { "auth": auth } } }))?.as_bytes())?;
                set("DOCKER_CONFIG", &conf.to_string_lossy());
                set("KILN_REGISTRY_USERNAME", user);
                set("KILN_REGISTRY_PASSWORD", t);
            }
        }
        "slack" => {
            if let Some(t) = i.token.as_deref() {
                if t.starts_with("xoxb-") || t.starts_with("xoxp-") {
                    set("SLACK_BOT_TOKEN", t);
                    if let Some(c) = cfg(i, "channel") {
                        set("SLACK_CHANNEL", c);
                    }
                } else {
                    set("SLACK_WEBHOOK_URL", t);
                }
            }
        }
        kind => {
            let name = TOKEN_ENV.iter().find(|(k, _)| *k == kind).map_or_else(|| format!("KILN_{}_TOKEN", safe(&i.name).to_ascii_uppercase()), |(_, v)| (*v).to_owned());
            if let Some(t) = i.token.as_deref() {
                set(&name, t);
            }
            for (key, var) in [("account_id", "CLOUDFLARE_ACCOUNT_ID"), ("team_id", "VERCEL_ORG_ID"), ("chat_id", "TELEGRAM_CHAT_ID"), ("url", "KILN_WEBHOOK_URL"), ("organization", "TF_CLOUD_ORGANIZATION"), ("hostname", "TF_CLOUD_HOSTNAME"), ("address", "NOMAD_ADDR")] {
                if let Some(v) = cfg(i, key).filter(|_| matches!((kind, key), ("cloudflare", "account_id") | ("vercel", "team_id") | ("telegram", "chat_id") | ("webhook", "url") | ("terraform", _) | ("nomad", "address"))) {
                    set(var, v);
                }
            }
            // Terraform's CLI reads a host's token from TF_TOKEN_<host>.
            if let (Some(t), "terraform") = (i.token.as_deref(), kind) {
                set(&format!("TF_TOKEN_{}", cfg(i, "hostname").unwrap_or("app.terraform.io").replace(['.', '-'], "_")), t);
            }
        }
    }
    if let Some(t) = i.token.as_deref() {
        a.masks.push(t.to_owned());
    }
    a.env = env.into_iter().collect();
    Ok(a)
}

/// Writes `bytes` to `path` where only this user reads it, whole or not at all.
pub fn write_private(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    let tmp = path.with_extension("tmp");
    {
        let mut o = std::fs::OpenOptions::new();
        o.write(true).create(true).truncate(true);
        #[cfg(unix)]
        std::os::unix::fs::OpenOptionsExt::mode(&mut o, 0o600);
        std::io::Write::write_all(&mut o.open(&tmp)?, bytes)?;
    }
    std::fs::rename(tmp, path)
}

/// An ID token for `audience`, from the leg's token service (`ACTIONS_ID_TOKEN_REQUEST_URL`).
pub async fn id_token(http: &crate::http::client::Client, url: &str, request_token: &str, audience: &str) -> anyhow::Result<String> {
    let sep = if url.contains('?') { '&' } else { '?' };
    let full = format!("{url}{sep}audience={}", crate::ci::cli::enc(audience));
    let r = http.get(&full).bearer(request_token).timeout(Duration::from_secs(30)).send().await?;
    let status = r.status();
    let v: Value = r.json(1 << 20).await.unwrap_or_default();
    anyhow::ensure!((200..300).contains(&status), "an ID token for {audience}: {}", v.get("message").and_then(Value::as_str).unwrap_or("refused"));
    v.get("value").and_then(Value::as_str).map(str::to_owned).ok_or_else(|| anyhow::anyhow!("the token service answered no token"))
}

/// A leg's ID token files, kept fresh while the leg runs: the tokens minted since the last
/// step, for its masker.
pub struct Refresh {
    files: Mutex<Vec<(PathBuf, String)>>,
    fresh: Mutex<Vec<String>>,
    url: String,
    request_token: String,
    http: Arc<crate::http::client::Client>,
}

impl Refresh {
    /// Keeps `files` fresh from now on, every four minutes, until the leg lets go of it.
    pub fn start(http: Arc<crate::http::client::Client>, url: String, request_token: String) -> Arc<Refresh> {
        let r = Arc::new(Refresh { files: Mutex::new(Vec::new()), fresh: Mutex::new(Vec::new()), url, request_token, http });
        let weak: Weak<Refresh> = Arc::downgrade(&r);
        crate::rt::spawn(async move {
            loop {
                crate::rt::sleep(Duration::from_secs(240)).await;
                let Some(r) = weak.upgrade() else { return };
                let files = r.files.lock().clone();
                for (path, aud) in files {
                    match id_token(&r.http, &r.url, &r.request_token, &aud).await {
                        Ok(t) => {
                            if write_private(&path, t.as_bytes()).is_ok() {
                                r.fresh.lock().push(t);
                            }
                        }
                        // Its tokens are revoked: the leg is over.
                        Err(_) => return,
                    }
                }
            }
        });
        r
    }

    /// Mints `files`' first tokens and keeps them fresh: the tokens, to mask.
    pub async fn add(&self, files: &[(PathBuf, String)]) -> anyhow::Result<Vec<String>> {
        let mut minted = Vec::new();
        for (path, aud) in files {
            let t = id_token(&self.http, &self.url, &self.request_token, aud).await?;
            write_private(path, t.as_bytes())?;
            minted.push(t);
            self.files.lock().push((path.clone(), aud.clone()));
        }
        Ok(minted)
    }

    /// Tokens minted since the last call.
    pub fn take_fresh(&self) -> Vec<String> {
        std::mem::take(&mut *self.fresh.lock())
    }
}

/// The runner's own binary in `<work>/_externals/kiln/` (containers see it at `/__e/kiln/`), so
/// a step can run `kiln auth` (W11-R04): linked where it can be, copied where it can't,
/// replaced when the runner is. (A container needs the libc the binary was built against.)
pub fn place_binary(work: &Path) -> Option<PathBuf> {
    let exe = std::env::current_exe().ok()?;
    let dir = work.join("_externals").join("kiln");
    let dst = dir.join(if cfg!(windows) { "kiln.exe" } else { "kiln" });
    let current = match (std::fs::metadata(&exe), std::fs::metadata(&dst)) {
        (Ok(x), Ok(y)) => x.len() == y.len() && x.modified().ok() <= y.modified().ok(),
        _ => false,
    };
    if !current {
        std::fs::create_dir_all(&dir).ok()?;
        let tmp = dir.join(format!(".kiln-{}", std::process::id()));
        let _ = std::fs::remove_file(&tmp);
        if std::fs::hard_link(&exe, &tmp).is_err() {
            std::fs::copy(&exe, &tmp).ok()?;
        }
        std::fs::rename(&tmp, &dst).ok()?;
    }
    Some(dir)
}

/// `kiln auth`: an integration's credentials from inside a leg (docs/ci.md W11-R04).
#[derive(Debug, clap::Args)]
pub struct AuthArgs {
    /// The integration, by name: one the leg's `integrations:` or a `kiln/auth@v1` step names.
    pub integration: String,
    /// Print the variables, `NAME=value` a line (the default): `kiln auth aws-prod >> "$GITHUB_ENV"`.
    #[arg(long)]
    pub print_env: bool,
    /// Run a command with them instead: `kiln auth aws-prod --exec aws s3 ls`.
    #[arg(long, num_args = 1.., allow_hyphen_values = true, value_name = "CMD")]
    pub exec: Vec<String>,
}

/// The leg's ID token for `audience`, through its token service's variables.
async fn leg_token(http: &crate::http::client::Client, audience: &str) -> anyhow::Result<String> {
    let url = std::env::var("ACTIONS_ID_TOKEN_REQUEST_URL").map_err(|_| anyhow::anyhow!("this leg gets no ID tokens: its job needs `permissions: id-token: write`"))?;
    let token = std::env::var("ACTIONS_ID_TOKEN_REQUEST_TOKEN").unwrap_or_default();
    id_token(http, &url, &token, audience).await
}

/// What `kiln auth` gives: short-lived credentials, exchanged here, for the federating kinds;
/// the stored token's variables for the rest. And the secrets in them, to mask.
async fn credentials(i: &Integration) -> anyhow::Result<(BTreeMap<String, String>, Vec<String>)> {
    use crate::ci::integrations::{aws, azure, gcp};
    let http = crate::http::client::Client::public()?;
    let mut env = BTreeMap::new();
    let mut secrets = Vec::new();
    let mut set = |k: &str, v: &str| {
        env.insert(k.to_owned(), v.to_owned());
    };
    match i.kind.as_str() {
        "aws" => {
            let role = cfg(i, "role_arn").ok_or_else(|| anyhow::anyhow!("`{}` has no role_arn", i.name))?;
            let jwt = leg_token(&http, &audience(i).unwrap_or_default()).await?;
            let session = session_name(&format!("kiln-{}", std::env::var("GITHUB_REPOSITORY").unwrap_or_default()));
            let c = aws::assume_role_with_web_identity(&aws::sts_endpoint(cfg(i, "region")), role, &session, &jwt, 3600).await?;
            set("AWS_ACCESS_KEY_ID", &c.access_key_id);
            set("AWS_SECRET_ACCESS_KEY", &c.secret_access_key);
            secrets.push(c.secret_access_key.clone());
            if let Some(t) = &c.session_token {
                set("AWS_SESSION_TOKEN", t);
                secrets.push(t.clone());
            }
            if let Some(r) = cfg(i, "region") {
                set("AWS_REGION", r);
                set("AWS_DEFAULT_REGION", r);
            }
        }
        "gcp" => {
            let provider = gcp_provider(cfg(i, "provider").ok_or_else(|| anyhow::anyhow!("`{}` has no provider", i.name))?).to_owned();
            let jwt = leg_token(&http, &audience(i).unwrap_or_default()).await?;
            let federated = gcp::exchange(&provider, &jwt).await?;
            let t = match cfg(i, "service_account") {
                Some(sa) => gcp::impersonate(sa, &federated.access_token).await?,
                None => federated,
            };
            set("CLOUDSDK_AUTH_ACCESS_TOKEN", &t.access_token);
            set("GOOGLE_OAUTH_ACCESS_TOKEN", &t.access_token);
            secrets.push(t.access_token.clone());
            if let Some(p) = cfg(i, "project") {
                for k in ["GOOGLE_CLOUD_PROJECT", "CLOUDSDK_CORE_PROJECT", "CLOUDSDK_PROJECT", "GCLOUD_PROJECT", "GCP_PROJECT"] {
                    set(k, p);
                }
            }
        }
        "azure" => {
            let (tenant, client) = (cfg(i, "tenant_id").ok_or_else(|| anyhow::anyhow!("`{}` has no tenant_id", i.name))?, cfg(i, "client_id").ok_or_else(|| anyhow::anyhow!("`{}` has no client_id", i.name))?);
            let jwt = leg_token(&http, azure::AUDIENCE).await?;
            let t = azure::exchange(tenant, client, azure::MANAGEMENT, &jwt).await?;
            set("AZURE_CLIENT_ID", client);
            set("AZURE_TENANT_ID", tenant);
            set("AZURE_ACCESS_TOKEN", &t.access_token);
            secrets.push(t.access_token.clone());
            if let Some(s) = cfg(i, "subscription_id") {
                set("AZURE_SUBSCRIPTION_ID", s);
                set("ARM_SUBSCRIPTION_ID", s);
            }
        }
        _ => {
            let dir = std::env::var_os("RUNNER_TEMP").map_or_else(std::env::temp_dir, PathBuf::from).join("_kiln_integrations");
            std::fs::create_dir_all(&dir)?;
            let a = apply(i, &dir, "kiln", &|p| p.to_string_lossy().into_owned())?;
            for (k, v) in &a.env {
                set(k, v);
            }
            secrets.extend(a.masks);
        }
    }
    Ok((env, secrets))
}

pub async fn cli(args: AuthArgs) -> anyhow::Result<()> {
    use std::io::Write as _;
    let path = std::env::var_os("KILN_INTEGRATIONS").ok_or_else(|| anyhow::anyhow!("kiln auth runs inside a leg that names its integrations (`integrations:`, or a `kiln/auth@v1` step)"))?;
    let list: Vec<Integration> = serde_json::from_slice(&std::fs::read(&path)?)?;
    let Some(i) = list.iter().find(|i| i.name == args.integration) else {
        anyhow::bail!("this leg has no integration `{}`; it has {}", args.integration, list.iter().map(|i| i.name.as_str()).collect::<Vec<_>>().join(", "))
    };
    let (env, secrets) = credentials(i).await?;
    let mut out = std::io::stdout().lock();
    // The runner masks what follows from here on.
    for s in secrets.iter().filter(|s| !s.is_empty()) {
        writeln!(out, "::add-mask::{s}")?;
    }
    if let Some((prog, rest)) = args.exec.split_first() {
        out.flush()?;
        let status = std::process::Command::new(prog).args(rest).envs(&env).status().map_err(|e| anyhow::anyhow!("{prog}: {e}"))?;
        anyhow::ensure!(status.success(), "{prog} exited with {}", status.code().map_or_else(|| "a signal".to_owned(), |c| c.to_string()));
        return Ok(());
    }
    let _ = args.print_env;
    for (k, v) in &env {
        writeln!(out, "{k}={v}")?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn integration(kind: &str, config: Value, token: Option<&str>) -> Integration {
        Integration { name: "prod one".into(), kind: kind.into(), mode: if token.is_some() { "token".into() } else { "oidc".into() }, config, token: token.map(str::to_owned), job: true }
    }

    #[test]
    fn w11_r03_env() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let plain = |p: &Path| p.to_string_lossy().into_owned();
        let aws = apply(&integration("aws", json!({ "role_arn": "arn:aws:iam::1:role/ci", "region": "eu-west-1" }), None), dir.path(), "kiln acme/app #7", &plain)?;
        assert_eq!(aws.env.get("AWS_ROLE_ARN").map(String::as_str), Some("arn:aws:iam::1:role/ci"));
        assert_eq!(aws.env.get("AWS_ROLE_SESSION_NAME").map(String::as_str), Some("kiln-acme-app-7"), "AWS's characters only");
        assert_eq!(aws.tokens.first().map(|(p, a)| (p.ends_with("prod_one.jwt"), a.as_str())), Some((true, "sts.amazonaws.com")));
        let gcp = apply(&integration("gcp", json!({ "provider": "projects/9/locations/global/workloadIdentityPools/p/providers/k", "service_account": "ci@x.iam.gserviceaccount.com", "project": "x" }), None), dir.path(), "s", &|_| "/__w/_temp/prod_one.jwt".into())?;
        let creds: Value = serde_json::from_str(&std::fs::read_to_string(gcp.env.get("GOOGLE_APPLICATION_CREDENTIALS").map(String::as_str).unwrap_or_default())?)?;
        assert_eq!(creds.pointer("/credential_source/file").and_then(Value::as_str), Some("/__w/_temp/prod_one.jwt"), "as a container's step sees it");
        assert_eq!(creds.get("audience").and_then(Value::as_str), Some("//iam.googleapis.com/projects/9/locations/global/workloadIdentityPools/p/providers/k"));
        assert!(creds.get("service_account_impersonation_url").and_then(Value::as_str).is_some_and(|u| u.contains("ci@x.iam.gserviceaccount.com:generateAccessToken")));
        assert_eq!(gcp.tokens.first().map(|(_, a)| a.as_str()), Some("https://iam.googleapis.com/projects/9/locations/global/workloadIdentityPools/p/providers/k"));
        let az = apply(&integration("azure", json!({ "tenant_id": "t", "client_id": "c" }), None), dir.path(), "s", &plain)?;
        assert_eq!((az.env.get("AZURE_TENANT_ID").map(String::as_str), az.env.get("ARM_USE_OIDC").map(String::as_str)), (Some("t"), Some("true")));
        let cf = apply(&integration("cloudflare", json!({ "account_id": "acc" }), Some("cf-token")), dir.path(), "s", &plain)?;
        assert_eq!((cf.env.get("CLOUDFLARE_API_TOKEN").map(String::as_str), cf.env.get("CLOUDFLARE_ACCOUNT_ID").map(String::as_str)), (Some("cf-token"), Some("acc")));
        assert_eq!(cf.masks, vec!["cf-token".to_owned()]);
        let other = apply(&integration("mystery", json!({}), Some("s3")), dir.path(), "s", &plain)?;
        assert_eq!(other.env.get("KILN_PROD_ONE_TOKEN").map(String::as_str), Some("s3"));
        let reg = apply(&integration("oci", json!({ "registry": "ghcr.io", "username": "bot" }), Some("pat")), dir.path(), "s", &plain)?;
        let conf: Value = serde_json::from_str(&std::fs::read_to_string(Path::new(reg.env.get("DOCKER_CONFIG").map(String::as_str).unwrap_or_default()).join("config.json"))?)?;
        assert!(conf.pointer("/auths/ghcr.io/auth").is_some());
        assert!(apply(&integration("aws", json!({}), None), dir.path(), "s", &plain).is_err(), "a role is needed");
        Ok(())
    }
}
