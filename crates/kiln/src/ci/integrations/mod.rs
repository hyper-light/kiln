//! Integrations and the credential broker (docs/ci.md W11; docs/cd-targets.md §6, §7.1): the
//! services kiln and its jobs work with, each configured once, shared by jobs and deployments.
//!
//! - **The registry** (W11-R01) is `ci_integrations`: a kind, a unique name, non-secret
//!   `config`, who may use it (a scope), and its verification state.
//! - **Credentials** an integration holds (a token, a password) are W09 secrets in the scope
//!   `('integration', <id>)`, named `TOKEN`. Jobs never resolve that scope; the server reads it
//!   for the integration alone ([`credentials`]).
//! - **Kinds** are the catalog below: what each needs, and how kiln authenticates to it. Kinds
//!   that federate (AWS, Google Cloud, Azure) exchange kiln's OIDC identity for short-lived
//!   credentials; the rest use a stored token.

pub mod aws;
pub mod azure;
pub mod cloudflare;
pub mod gcp;
pub mod oci;
pub mod vercel;

use std::collections::HashMap;

use http::StatusCode;
use rusqlite::{OptionalExtension, params};
use serde::Serialize;
use serde_json::{Value, json};

use crate::hub::Hub;
use crate::http::server::{self as srv, Response};

use super::Fail;
use super::secrets::{self, Scope};
use kiln_ci::secrets::ScopeKind;

/// Where a service's requests go (`aws:ecs`, `gcp:sts`): its real endpoint, or a fake a test put
/// in its place with [`override_endpoint`].
pub fn endpoint(service: &str, real: impl FnOnce() -> String) -> String {
    let found = overrides().lock().get(service).cloned();
    // Tests never reach a real cloud: one that forgot to fake a service gets nowhere.
    if cfg!(test) {
        return found.unwrap_or_else(|| "http://127.0.0.1:9".into());
    }
    found.unwrap_or_else(real)
}

fn overrides() -> &'static parking_lot::Mutex<HashMap<String, String>> {
    static O: std::sync::OnceLock<parking_lot::Mutex<HashMap<String, String>>> = std::sync::OnceLock::new();
    O.get_or_init(|| parking_lot::Mutex::new(HashMap::new()))
}

/// Sends `service`'s requests to `base` instead (a fake, in tests).
pub fn override_endpoint(service: &str, base: &str) {
    overrides().lock().insert(service.to_owned(), base.trim_end_matches('/').to_owned());
}

/// How kiln authenticates to a kind's service (W11-R02).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum Mode {
    /// kiln's OIDC identity, exchanged for short-lived credentials.
    Oidc,
    /// A stored token.
    Token,
    /// A stored token, or none (public registries).
    Optional,
}

/// One field of a kind's configuration.
#[derive(Clone, Copy, Debug, Serialize)]
pub struct Field {
    pub name: &'static str,
    pub required: bool,
    pub help: &'static str,
}

/// One kind of integration.
#[derive(Clone, Copy, Debug, Serialize)]
pub struct KindInfo {
    pub kind: &'static str,
    pub title: &'static str,
    pub category: &'static str,
    pub mode: Mode,
    pub fields: &'static [Field],
    /// What the stored secret is, for kinds that store one.
    pub secret: Option<&'static str>,
    /// Whether kiln can use it yet (the catalog lists what's coming too).
    pub built: bool,
}

const fn f(name: &'static str, required: bool, help: &'static str) -> Field {
    Field { name, required, help }
}

/// The catalog (docs/ci.md Appendix C; docs/cd-targets.md §3).
pub const KINDS: &[KindInfo] = &[
    KindInfo {
        kind: "oci",
        title: "Container registry",
        category: "registry",
        mode: Mode::Optional,
        fields: &[
            f("registry", true, "as image references name it: docker.io, ghcr.io, quay.io, registry.example.com:5000"),
            f("username", false, "for a private registry, or Docker Hub pulls without the anonymous limit"),
            f("insecure", false, "true for a registry that speaks plain HTTP (a local network, a test)"),
        ],
        secret: Some("access token or password"),
        built: true,
    },
    KindInfo {
        kind: "aws",
        title: "AWS",
        category: "cloud",
        mode: Mode::Oidc,
        fields: &[f("role_arn", true, "the IAM role kiln assumes"), f("region", false, "the default region"), f("audience", false, "the token audience the role trusts (sts.amazonaws.com)")],
        secret: None,
        built: true,
    },
    KindInfo {
        kind: "gcp",
        title: "Google Cloud",
        category: "cloud",
        mode: Mode::Oidc,
        fields: &[
            f("provider", true, "projects/<number>/locations/global/workloadIdentityPools/<pool>/providers/<provider>"),
            f("service_account", false, "a service account to impersonate, if any"),
            f("project", false, "the default project"),
        ],
        secret: None,
        built: true,
    },
    KindInfo {
        kind: "azure",
        title: "Azure",
        category: "cloud",
        mode: Mode::Oidc,
        fields: &[f("tenant_id", true, "the Entra tenant"), f("client_id", true, "the app registration with kiln's federated credential"), f("subscription_id", false, "the default subscription")],
        secret: None,
        built: true,
    },
    // CD providers (docs/deploy-api.md §8.3): an Argo CD instance kiln deploys through.
    KindInfo {
        kind: "argocd",
        title: "Argo CD",
        category: "cd",
        mode: Mode::Token,
        fields: &[
            f("url", true, "its API server: https://argocd.example.com"),
            f("namespace", false, "where its Applications live (argocd)"),
            f("ca", false, "the PEM of the CA its certificate is signed by, when it isn't a public one"),
        ],
        secret: Some("an account's API token (argocd account generate-token --account <name>)"),
        built: true,
    },
    KindInfo { kind: "cloudflare", title: "Cloudflare", category: "hosting", mode: Mode::Token, fields: &[f("account_id", true, "the account Workers and Pages live in")], secret: Some("API token"), built: true },
    // Chat, paging and webhooks: where notifications go (docs/ci.md W11-R06, W19-R06).
    KindInfo { kind: "slack", title: "Slack", category: "notify", mode: Mode::Token, fields: &[f("channel", false, "the channel a bot token posts to when a rule names none (#ci)")], secret: Some("incoming webhook URL, or a bot token (xoxb-…)"), built: true },
    KindInfo { kind: "discord", title: "Discord", category: "notify", mode: Mode::Token, fields: &[], secret: Some("webhook URL"), built: true },
    KindInfo { kind: "teams", title: "Microsoft Teams", category: "notify", mode: Mode::Token, fields: &[], secret: Some("Workflows webhook URL"), built: true },
    KindInfo { kind: "telegram", title: "Telegram", category: "notify", mode: Mode::Token, fields: &[f("chat_id", false, "the chat a rule that names none posts to")], secret: Some("bot token"), built: true },
    KindInfo { kind: "pagerduty", title: "PagerDuty", category: "notify", mode: Mode::Token, fields: &[], secret: Some("Events API v2 routing key"), built: true },
    KindInfo { kind: "opsgenie", title: "Opsgenie", category: "notify", mode: Mode::Token, fields: &[], secret: Some("API key"), built: true },
    KindInfo { kind: "webhook", title: "Webhook", category: "notify", mode: Mode::Optional, fields: &[f("url", true, "where kiln POSTs each event, as JSON"), f("events", false, "kiln events to send as they happen: run, job, agent, queue, cd_plan, cd_apps, cd_app_deleted, or * (comma-separated); without, only notify rules use it")], secret: Some("signing key (X-Kiln-Signature-256: sha256=<HMAC of the body>)"), built: true },
    KindInfo { kind: "vercel", title: "Vercel", category: "hosting", mode: Mode::Token, fields: &[f("team_id", false, "the team, when not a personal account")], secret: Some("access token"), built: true },
    KindInfo { kind: "railway", title: "Railway", category: "hosting", mode: Mode::Token, fields: &[f("workspace", false, "the workspace")], secret: Some("API token"), built: false },
    KindInfo { kind: "fly", title: "Fly.io", category: "hosting", mode: Mode::Token, fields: &[f("org", false, "the organization slug")], secret: Some("access token"), built: false },
    KindInfo { kind: "netlify", title: "Netlify", category: "hosting", mode: Mode::Token, fields: &[f("account", false, "the team slug")], secret: Some("personal access token"), built: false },
    KindInfo { kind: "render", title: "Render", category: "hosting", mode: Mode::Token, fields: &[f("owner_id", false, "the workspace")], secret: Some("API key"), built: false },
    KindInfo { kind: "heroku", title: "Heroku", category: "hosting", mode: Mode::Token, fields: &[], secret: Some("API key"), built: false },
    KindInfo { kind: "digitalocean", title: "DigitalOcean", category: "hosting", mode: Mode::Token, fields: &[], secret: Some("personal access token"), built: false },
    KindInfo {
        kind: "nomad",
        title: "Nomad",
        category: "infrastructure",
        mode: Mode::Token,
        fields: &[f("address", true, "https://nomad.example.com:4646"), f("namespace", false, "default"), f("region", false, "the region")],
        secret: Some("ACL token"),
        built: false,
    },
    KindInfo { kind: "terraform", title: "HCP Terraform", category: "infrastructure", mode: Mode::Token, fields: &[f("organization", true, "the organization"), f("hostname", false, "app.terraform.io, or a Terraform Enterprise host")], secret: Some("team token"), built: false },
    KindInfo { kind: "pulumi", title: "Pulumi Cloud", category: "infrastructure", mode: Mode::Token, fields: &[f("organization", true, "the organization")], secret: Some("access token"), built: false },
];

/// The catalog entry for `kind`.
pub fn kind(kind: &str) -> Option<&'static KindInfo> {
    KINDS.iter().find(|k| k.kind == kind)
}

/// An integration as stored and shown: never its secret.
#[derive(Clone, Debug, Serialize)]
pub struct Integration {
    pub id: String,
    pub kind: String,
    pub name: String,
    pub config: Value,
    /// Whether it holds a secret.
    pub has_secret: bool,
    pub scope: String,
    pub created_by: String,
    pub created_at: i64,
    pub updated_at: i64,
    pub verified_at: Option<i64>,
    pub last_error: Option<String>,
    pub last_used_at: Option<i64>,
}

const SECRET_NAME: &str = "TOKEN";

fn secret_scope(id: &str) -> Scope {
    Scope { kind: ScopeKind::Integration, id: id.to_owned() }
}

/// A name people and files refer to an integration by: lower case, digits and `-`.
fn check_name(name: &str) -> Result<String, String> {
    let n = name.trim().to_ascii_lowercase();
    let ok = !n.is_empty() && n.len() <= 63 && n.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-') && !n.starts_with('-');
    if ok { Ok(n) } else { Err(format!("`{name}` is not an integration name: lower case letters, digits and `-`, at most 63")) }
}

/// Checks `config` against `kind`'s fields: required ones present, unknown ones refused.
fn check_config(info: &KindInfo, config: &Value) -> Result<(), String> {
    let obj = config.as_object().ok_or("the configuration is an object")?;
    for k in obj.keys() {
        if !info.fields.iter().any(|f| f.name == k) {
            let known: Vec<&str> = info.fields.iter().map(|f| f.name).collect();
            return Err(format!("{} has no `{k}` (it takes {})", info.title, if known.is_empty() { "nothing".to_owned() } else { known.join(", ") }));
        }
    }
    for f in info.fields.iter().filter(|f| f.required) {
        if obj.get(f.name).and_then(Value::as_str).is_none_or(|s| s.trim().is_empty()) {
            return Err(format!("{} needs `{}`: {}", info.title, f.name, f.help));
        }
    }
    if info.kind == "webhook" {
        crate::ci::webhooks::check_events(config)?;
    }
    if info.kind == "oci"
        && let Some(r) = obj.get("registry").and_then(Value::as_str)
    {
        oci::parse_reference(&format!("{}/probe/probe", r.trim())).map_err(|_| format!("`{r}` is not a registry name (docker.io, ghcr.io, registry.example.com:5000)"))?;
    }
    Ok(())
}

fn row(r: &rusqlite::Row<'_>) -> rusqlite::Result<Integration> {
    let config: String = r.get(3)?;
    let scope_kind: String = r.get(5)?;
    let scope_id: String = r.get(6)?;
    Ok(Integration {
        id: r.get(0)?,
        kind: r.get(1)?,
        name: r.get(2)?,
        config: serde_json::from_str(&config).unwrap_or(Value::Null),
        has_secret: r.get::<_, Option<String>>(4)?.is_some(),
        scope: if scope_id.is_empty() { scope_kind } else { format!("{scope_kind}:{scope_id}") },
        created_by: r.get(7)?,
        created_at: r.get(8)?,
        updated_at: r.get(9)?,
        verified_at: r.get(10)?,
        last_error: r.get(11)?,
        last_used_at: r.get(12)?,
    })
}

const COLUMNS: &str = "id, kind, name, config, secret_ref, scope_kind, scope_id, created_by, created_at, updated_at, verified_at, last_error, last_used_at";

/// Whether `repo`'s runs may use integration `i`: its scope holds the repository.
pub fn usable_by(i: &Integration, repo: &crate::store::Repo) -> bool {
    let owner = repo.name.split('/').next().unwrap_or_default();
    match i.scope.split_once(':') {
        None => i.scope == "server" || i.scope.is_empty(),
        Some(("org", o)) => o.eq_ignore_ascii_case(owner),
        Some(("repo", r)) => r == repo.id || r.eq_ignore_ascii_case(&repo.name),
        _ => false,
    }
}

pub async fn list(hub: &Hub) -> anyhow::Result<Vec<Integration>> {
    hub.store
        .call(|c| {
            let mut q = c.prepare(&format!("SELECT {COLUMNS} FROM ci_integrations ORDER BY name"))?;
            let rows = q.query_map([], row)?;
            Ok(rows.collect::<Result<Vec<_>, _>>()?)
        })
        .await
}

/// Whether any integration federates kiln's identity (`aws`, `gcp`, `azure`): then the issuer's
/// documents are served, since those accounts' syncs mint tokens. Only reads.
pub async fn federates(hub: &Hub) -> bool {
    hub.store
        .call(|c| Ok(c.query_row("SELECT 1 FROM ci_integrations WHERE kind IN ('aws', 'gcp', 'azure') LIMIT 1", [], |_| Ok(())).optional()?.is_some()))
        .await
        .unwrap_or(false)
}

pub async fn get(hub: &Hub, name: &str) -> anyhow::Result<Option<Integration>> {
    let name = name.trim().to_ascii_lowercase();
    hub.store.call(move |c| Ok(c.query_row(&format!("SELECT {COLUMNS} FROM ci_integrations WHERE name = ?1"), params![name], row).optional()?)).await
}

/// Creates an integration; `secret` is its token or password, kept as a W09 secret.
pub async fn create(hub: &Hub, kind_name: &str, name: &str, config: Value, secret: Option<&str>, scope: Option<&str>, by: &str) -> Result<Integration, String> {
    let info = kind(kind_name).ok_or_else(|| format!("no integration kind `{kind_name}` (kinds: {})", KINDS.iter().map(|k| k.kind).collect::<Vec<_>>().join(", ")))?;
    let name = check_name(name)?;
    check_config(info, &config)?;
    if secret.is_some_and(|s| !s.trim().is_empty()) && info.secret.is_none() {
        return Err(format!("{} federates with kiln's identity: it stores no secret", info.title));
    }
    let scope = match scope.map(str::trim).filter(|s| !s.is_empty()) {
        Some(s) => secrets::parse_scope(hub, s).await?,
        None => Scope { kind: ScopeKind::Server, id: String::new() },
    };
    let id = crate::util::new_id();
    let secret = secret.map(str::trim).filter(|s| !s.is_empty());
    if let Some(s) = secret {
        secrets::set(hub, &secret_scope(&id), SECRET_NAME, s, by).await?;
    }
    let (row_id, n, k, cfg, has, by_s) = (id.clone(), name.clone(), info.kind.to_owned(), config.to_string(), secret.is_some(), by.to_owned());
    let inserted = hub
        .store
        .call(move |c| {
            let now = crate::util::now_unix();
            let exists: bool = c.query_row("SELECT EXISTS(SELECT 1 FROM ci_integrations WHERE name = ?1)", params![n], |r| r.get(0))?;
            if exists {
                anyhow::bail!("an integration named `{n}` exists");
            }
            c.execute(
                "INSERT INTO ci_integrations (id, kind, name, config, secret_ref, scope_kind, scope_id, created_by, created_at, updated_at) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?9)",
                params![row_id, k, n, cfg, has.then_some(SECRET_NAME), scope.kind.as_str(), scope.id, by_s, now],
            )?;
            Ok(())
        })
        .await;
    if let Err(e) = inserted {
        let _ = secrets::delete(hub, &secret_scope(&id), SECRET_NAME).await;
        return Err(format!("{e:#}"));
    }
    hub.store.audit(by.to_owned(), "integration.create".into(), format!("{name} ({})", info.kind)).await;
    get(hub, &name).await.map_err(|e| format!("{e:#}"))?.ok_or_else(|| "the integration vanished".into())
}

/// Changes an integration's configuration and/or secret (`Some("")` removes the secret).
pub async fn update(hub: &Hub, name: &str, config: Option<Value>, secret: Option<&str>, by: &str) -> Result<Integration, String> {
    let have = get(hub, name).await.map_err(|e| format!("{e:#}"))?.ok_or_else(|| format!("no integration `{name}`"))?;
    let info = kind(&have.kind).ok_or("its kind is no longer known")?;
    if let Some(c) = &config {
        check_config(info, c)?;
    }
    match secret.map(str::trim) {
        Some("") => {
            secrets::delete(hub, &secret_scope(&have.id), SECRET_NAME).await.map_err(|e| format!("{e:#}"))?;
        }
        Some(s) => {
            if info.secret.is_none() {
                return Err(format!("{} federates with kiln's identity: it stores no secret", info.title));
            }
            secrets::set(hub, &secret_scope(&have.id), SECRET_NAME, s, by).await?;
        }
        None => {}
    }
    let (id, cfg, has) = (have.id.clone(), config.map(|c| c.to_string()), secret.map(|s| !s.trim().is_empty()));
    hub.store
        .call(move |c| {
            let now = crate::util::now_unix();
            if let Some(cfg) = cfg {
                c.execute("UPDATE ci_integrations SET config = ?2, verified_at = NULL, updated_at = ?3 WHERE id = ?1", params![id, cfg, now])?;
            }
            if let Some(has) = has {
                c.execute("UPDATE ci_integrations SET secret_ref = ?2, verified_at = NULL, updated_at = ?3 WHERE id = ?1", params![id, has.then_some(SECRET_NAME), now])?;
            }
            Ok(())
        })
        .await
        .map_err(|e| format!("{e:#}"))?;
    hub.store.audit(by.to_owned(), "integration.update".into(), have.name.clone()).await;
    get(hub, &have.name).await.map_err(|e| format!("{e:#}"))?.ok_or_else(|| "the integration vanished".into())
}

pub async fn delete(hub: &Hub, name: &str, by: &str) -> anyhow::Result<bool> {
    let Some(have) = get(hub, name).await? else { return Ok(false) };
    secrets::delete(hub, &secret_scope(&have.id), SECRET_NAME).await?;
    let id = have.id.clone();
    hub.store.call(move |c| Ok(c.execute("DELETE FROM ci_integrations WHERE id = ?1", params![id])?)).await?;
    hub.store.audit(by.to_owned(), "integration.delete".into(), have.name).await;
    Ok(true)
}

/// The integration's stored secret, for the server's own use.
/// A token integration's stored token (`cloudflare`, `vercel`, …), for the providers that act
/// through it.
pub async fn token(hub: &Hub, i: &Integration) -> anyhow::Result<Option<String>> {
    secret(hub, i).await
}

async fn secret(hub: &Hub, i: &Integration) -> anyhow::Result<Option<String>> {
    if !i.has_secret {
        return Ok(None);
    }
    secrets::reveal(hub, &secret_scope(&i.id), SECRET_NAME).await
}

async fn mark(hub: &Hub, id: &str, result: &Result<String, String>) {
    let (id, ok, err) = (id.to_owned(), result.is_ok(), result.as_ref().err().cloned());
    let _ = hub
        .store
        .call(move |c| {
            let now = crate::util::now_unix();
            if ok {
                c.execute("UPDATE ci_integrations SET verified_at = ?2, last_error = NULL WHERE id = ?1", params![id, now])?;
            } else {
                c.execute("UPDATE ci_integrations SET last_error = ?2 WHERE id = ?1", params![id, err])?;
            }
            Ok(())
        })
        .await;
}

/// A registry reached through an `oci` integration: its credentials, ready to use.
pub async fn registry(hub: &Hub, name: &str) -> anyhow::Result<oci::Registry> {
    let i = get(hub, name).await?.ok_or_else(|| anyhow::anyhow!("no integration `{name}`"))?;
    anyhow::ensure!(i.kind == "oci", "integration `{name}` is {}, not a registry", i.kind);
    registry_of(hub, &i).await
}

async fn registry_of(hub: &Hub, i: &Integration) -> anyhow::Result<oci::Registry> {
    let reg = i.config.get("registry").and_then(Value::as_str).unwrap_or(oci::DOCKER_HUB).trim().to_ascii_lowercase();
    let reg = if reg == "index.docker.io" || reg == "registry-1.docker.io" { oci::DOCKER_HUB.to_owned() } else { reg };
    let insecure = i.config.get("insecure").is_some_and(|v| v.as_bool() == Some(true) || v.as_str() == Some("true"));
    let creds = match (i.config.get("username").and_then(Value::as_str).map(str::trim).filter(|u| !u.is_empty()), secret(hub, i).await?) {
        (Some(u), Some(p)) => Some((u.to_owned(), p)),
        // A token without a username: registries that take any username with a token (GHCR).
        (None, Some(p)) => Some(("kiln".to_owned(), p)),
        _ => None,
    };
    let id = i.id.clone();
    let _ = hub.store.call(move |c| Ok(c.execute("UPDATE ci_integrations SET last_used_at = ?2 WHERE id = ?1", params![id, crate::util::now_unix()])?)).await;
    Ok(oci::Registry::for_name(&reg, creds, insecure))
}

/// The registry integration that serves `registry` (as references name it), if one is set up:
/// the one whose `registry` matches, preferring one with credentials.
pub async fn registry_for(hub: &Hub, registry: &str) -> anyhow::Result<Option<oci::Registry>> {
    let all = list(hub).await?;
    let mut matching: Vec<&Integration> = all
        .iter()
        .filter(|i| i.kind == "oci" && i.config.get("registry").and_then(Value::as_str).is_some_and(|r| {
            let r = r.trim().to_ascii_lowercase();
            r == registry || (registry == oci::DOCKER_HUB && (r == "index.docker.io" || r == "registry-1.docker.io"))
        }))
        .collect();
    matching.sort_by_key(|i| !i.has_secret);
    match matching.first() {
        Some(i) => Ok(Some(registry_of(hub, i).await?)),
        None => Ok(None),
    }
}

/// Checks an integration against its service, and records the result.
/// Whose identity a check runs with, for kinds that federate: the subject a role or a federated
/// credential trusts (`repo:<repository>:environment:<environment>`). By default kiln's first
/// repository.
#[derive(Clone, Debug, Default, serde::Deserialize)]
pub struct VerifyAs {
    #[serde(default)]
    pub repository: Option<String>,
    #[serde(default)]
    pub environment: Option<String>,
}

pub async fn verify(hub: &Hub, name: &str, as_: &VerifyAs) -> anyhow::Result<Result<String, String>> {
    let i = get(hub, name).await?.ok_or_else(|| anyhow::anyhow!("no integration `{name}`"))?;
    let info = kind(&i.kind).ok_or_else(|| anyhow::anyhow!("unknown kind {}", i.kind))?;
    let result = if !info.built {
        Err(format!("{} integrations aren't built yet: kiln stores this one, and uses it once they are", info.title))
    } else {
        match i.kind.as_str() {
            "oci" => match registry_of(hub, &i).await {
                // A registry someone named: checked only where requests may go (W22-AC07).
                Ok(r) => match hub.cfg.ci.reach().check_url(&r.base).await {
                    Ok(()) => {
                        // Docker Hub scopes tokens to a repository: ask for one under the user's
                        // namespace, which any account may pull from.
                        let repo = r.credentials.as_ref().filter(|_| r.name == oci::DOCKER_HUB).map(|(u, _)| format!("{u}/probe"));
                        oci::verify(&r, repo.as_deref()).await.map_err(|e| format!("{e:#}"))
                    }
                    Err(e) => Err(format!("{e:#}")),
                },
                Err(e) => Err(format!("{e:#}")),
            },
            "aws" | "gcp" | "azure" => verify_federated(hub, &i, as_).await,
            k if crate::ci::notify::CHANNELS.contains(&k) => crate::ci::notify::verify(hub, &i).await,
            "vercel" => match secret(hub, &i).await {
                Ok(Some(token)) => vercel::verify(&token, i.config.get("team_id").and_then(Value::as_str)).await.map_err(|e| format!("{e:#}")),
                Ok(None) => Err("no token stored".into()),
                Err(e) => Err(format!("{e:#}")),
            },
            "cloudflare" => match secret(hub, &i).await {
                Ok(Some(token)) => cloudflare::verify(&token, i.config.get("account_id").and_then(Value::as_str)).await.map_err(|e| format!("{e:#}")),
                Ok(None) => Err("no token stored".into()),
                Err(e) => Err(format!("{e:#}")),
            },
            "argocd" => crate::deploy::argocd::verify(hub, &i.name).await,
            other => Err(format!("no check for {other} yet")),
        }
    };
    mark(hub, &i.id, &result).await;
    Ok(result)
}

/// A real exchange of kiln's identity, as a sync's would be: which subject, and what the cloud
/// said, with what to check when it refused.
async fn verify_federated(hub: &Hub, i: &Integration, as_: &VerifyAs) -> Result<String, String> {
    let repository = match as_.repository.clone().filter(|r| !r.trim().is_empty()) {
        Some(r) => r,
        None => hub.store.repos().await.ok().and_then(|r| r.into_iter().find(|r| r.enabled).map(|r| r.name)).unwrap_or_else(|| "kiln/kiln".into()),
    };
    let environment = as_.environment.clone().filter(|e| !e.trim().is_empty());
    let w = kiln_ci::oidc::Workload { repository: &repository, environment: environment.as_deref(), actor: "kiln", source: "cd", trigger: Some("verify"), ..Default::default() };
    let subject = match &environment {
        Some(e) => format!("repo:{repository}:environment:{e}"),
        None => format!("repo:{repository}:ref:"),
    };
    let explain = |e: anyhow::Error, advice: Option<&str>| match advice {
        Some(a) => format!("{e:#} — {a} (subject {subject})"),
        None => format!("{e:#} (subject {subject})"),
    };
    match i.kind.as_str() {
        "aws" => match aws::credentials(hub, i, &w).await {
            Ok(c) => Ok(format!("assumed the role as {subject}: temporary key {}…", c.access_key_id.chars().take(8).collect::<String>())),
            Err(e) => {
                let advice = aws::advice(&e);
                Err(explain(e, advice))
            }
        },
        "gcp" => match gcp::token(hub, i, &w).await {
            Ok(_) => Ok(format!("Google Cloud accepted kiln's token as {subject}")),
            Err(e) => {
                let advice = gcp::advice(&e);
                Err(explain(e, advice))
            }
        },
        _ => match azure::token(hub, i, &w, None).await {
            Ok(_) => Ok(format!("signed in to Azure as {subject}")),
            Err(e) => {
                let advice = azure::advice(&e);
                Err(explain(e, advice))
            }
        },
    }
}

// ------------------------------------------------------------------ API

fn fail(status: StatusCode, m: impl Into<String>) -> Fail {
    (status, m.into())
}

#[derive(serde::Deserialize)]
struct Create {
    kind: String,
    name: String,
    #[serde(default)]
    config: Value,
    #[serde(default)]
    secret: Option<String>,
    #[serde(default)]
    scope: Option<String>,
}

#[derive(serde::Deserialize)]
struct Patch {
    #[serde(default)]
    config: Option<Value>,
    #[serde(default)]
    secret: Option<String>,
}

/// `/api/ci/integrations…`.
pub async fn route(hub: &Hub, method: &http::Method, rest: &[&str], _q: &HashMap<String, String>, body: hyper::body::Incoming) -> Result<Response, Fail> {
    use http::Method;
    let internal = |e: anyhow::Error| fail(StatusCode::INTERNAL_SERVER_ERROR, format!("{e:#}"));
    match (method, rest) {
        (&Method::GET, ["integrations"]) => {
            let all = list(hub).await.map_err(internal)?;
            Ok(srv::json(StatusCode::OK, &json!({ "kinds": KINDS, "integrations": all })))
        }
        (&Method::POST, ["integrations"]) => {
            let c: Create = srv::body_json(body, 256 << 10).await.map_err(|e| fail(StatusCode::BAD_REQUEST, format!("{e:#}")))?;
            let config = if c.config.is_null() { json!({}) } else { c.config };
            let i = create(hub, &c.kind, &c.name, config, c.secret.as_deref(), c.scope.as_deref(), "admin").await.map_err(|e| fail(StatusCode::UNPROCESSABLE_ENTITY, e))?;
            Ok(srv::json(StatusCode::CREATED, &i))
        }
        (&Method::GET, ["integrations", "kinds"]) => Ok(srv::json(StatusCode::OK, &json!({ "kinds": KINDS }))),
        (&Method::GET, ["integrations", name]) => match get(hub, name).await.map_err(internal)? {
            Some(i) => Ok(srv::json(StatusCode::OK, &i)),
            None => Err(fail(StatusCode::NOT_FOUND, format!("no integration `{name}`"))),
        },
        (&Method::PATCH, ["integrations", name]) => {
            let p: Patch = srv::body_json(body, 256 << 10).await.map_err(|e| fail(StatusCode::BAD_REQUEST, format!("{e:#}")))?;
            let i = update(hub, name, p.config, p.secret.as_deref(), "admin").await.map_err(|e| fail(StatusCode::UNPROCESSABLE_ENTITY, e))?;
            Ok(srv::json(StatusCode::OK, &i))
        }
        (&Method::DELETE, ["integrations", name]) => {
            if delete(hub, name, "admin").await.map_err(internal)? {
                Ok(srv::status(StatusCode::NO_CONTENT))
            } else {
                Err(fail(StatusCode::NOT_FOUND, format!("no integration `{name}`")))
            }
        }
        (&Method::POST, ["integrations", name, "verify"]) => {
            // Optional: whose identity a federated check runs with.
            let bytes = srv::body_bytes(body, 64 << 10).await.map_err(|e| fail(StatusCode::BAD_REQUEST, format!("{e:#}")))?;
            let as_: VerifyAs = if bytes.is_empty() { VerifyAs::default() } else { serde_json::from_slice(&bytes).map_err(|e| fail(StatusCode::BAD_REQUEST, format!("{e}")))? };
            let r = verify(hub, name, &as_).await.map_err(|e| fail(StatusCode::NOT_FOUND, format!("{e:#}")))?;
            Ok(srv::json(StatusCode::OK, &match r {
                Ok(message) => json!({ "ok": true, "message": message }),
                Err(error) => json!({ "ok": false, "error": error }),
            }))
        }
        _ => Err(fail(StatusCode::NOT_FOUND, "no such route")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_and_configs_are_checked() {
        assert_eq!(check_name(" Docker-Hub "), Ok("docker-hub".to_owned()));
        assert!(check_name("no_underscores").is_err());
        assert!(check_name("-lead").is_err());
        let oci = kind("oci").copied();
        let Some(oci) = oci else { return };
        assert!(check_config(&oci, &json!({ "registry": "docker.io", "username": "acme" })).is_ok());
        assert!(check_config(&oci, &json!({ "username": "acme" })).is_err_and(|e| e.contains("needs `registry`")));
        assert!(check_config(&oci, &json!({ "registry": "docker.io", "password": "x" })).is_err_and(|e| e.contains("has no `password`")));
        assert!(check_config(&oci, &json!({ "registry": "https://ghcr.io" })).is_err());
        assert!(KINDS.iter().all(|k| k.mode != Mode::Oidc || k.secret.is_none()), "federating kinds store no secret");
    }
}
