//! `kiln secrets` and `kiln vars` (docs/ci.md W09-R11), over the server's API. A scope is
//! written `server` (the default), `org:<owner>`, `repo:<owner/name>`, `workflow:<repo>:<path>`
//! or `env:<repo>:<environment>`.

use std::io::Read as _;
use std::path::PathBuf;
use std::time::Duration;

use anyhow::{Context as _, Result, bail};
use serde_json::{Value, json};

use crate::config::Config;
use crate::http::client::{Client, Method};

#[derive(clap::Args)]
pub struct SecretsArgs {
    /// The server's URL, e.g. http://localhost:7070.
    #[arg(long, env = "KILN_URL", global = true)]
    url: Option<String>,
    /// The server's admin token.
    #[arg(long, env = "KILN_ADMIN_TOKEN", hide_env_values = true, global = true, allow_hyphen_values = true)]
    token: Option<String>,
    #[command(subcommand)]
    cmd: SecretsCmd,
}

#[derive(clap::Subcommand)]
enum SecretsCmd {
    /// The secrets of a scope: names, sizes and last use, never values.
    List {
        #[arg(long, default_value = "server")]
        scope: String,
    },
    /// Set a secret from a file, or from stdin (one trailing newline is dropped); or say where
    /// its value comes from at leg start, never stored (`--from integration:aws-prod`,
    /// `aws-sm:<arn>`, `gcp-sm:projects/…/secrets/…`, `azure-kv:<vault>/<secret>`).
    Set {
        name: String,
        #[arg(long, default_value = "server")]
        scope: String,
        #[arg(long, conflicts_with = "from")]
        from_file: Option<std::path::PathBuf>,
        #[arg(long)]
        from: Option<String>,
    },
    Delete {
        name: String,
        #[arg(long, default_value = "server")]
        scope: String,
    },
    /// Set every `NAME=value` of a dotenv file.
    Import {
        #[arg(long)]
        dotenv: std::path::PathBuf,
        #[arg(long, default_value = "server")]
        scope: String,
    },
    /// Re-seal every secret under a new data key.
    RotateKey,
}

#[derive(clap::Args)]
pub struct VarsArgs {
    #[arg(long, env = "KILN_URL", global = true)]
    url: Option<String>,
    #[arg(long, env = "KILN_ADMIN_TOKEN", hide_env_values = true, global = true, allow_hyphen_values = true)]
    token: Option<String>,
    #[command(subcommand)]
    cmd: VarsCmd,
}

#[derive(clap::Subcommand)]
enum VarsCmd {
    List {
        #[arg(long, default_value = "server")]
        scope: String,
    },
    Set {
        name: String,
        value: String,
        #[arg(long, default_value = "server")]
        scope: String,
    },
    Delete {
        name: String,
        #[arg(long, default_value = "server")]
        scope: String,
    },
}

/// A kiln server's API, as its admin (or a personal access token).
pub(crate) struct Api {
    client: Client,
    url: String,
    token: String,
}

impl Api {
    pub(crate) async fn connect(cfg: &Config, url: Option<&str>, token: Option<&str>) -> Result<Api> {
        let t = crate::access::resolve(cfg, url, token).await?;
        Ok(Api { client: Client::public()?, url: t.url, token: t.token })
    }

    /// A server this process runs (`kiln workflow run`'s own).
    pub(crate) fn local(url: &str, token: &str) -> Result<Api> {
        Ok(Api { client: Client::public()?, url: url.trim_end_matches('/').to_owned(), token: token.to_owned() })
    }

    /// The server's URL.
    pub(crate) fn url(&self) -> &str {
        &self.url
    }

    /// A WebSocket to `path` (from `/api`), as this caller.
    pub(crate) async fn websocket(&self, path: &str) -> Result<crate::http::ws::ClientSocket> {
        let auth = format!("Bearer {}", self.token);
        self.client.websocket_with(&format!("{}{path}", self.url), &[("authorization", auth.as_str())], 16 << 20).await
    }

    /// A CI route (`/api/ci<path>`).
    pub(crate) async fn call(&self, method: Method, path: &str, body: Option<Value>) -> Result<Value> {
        self.call_api(method, &format!("/api/ci{path}"), body).await
    }

    /// `POST /api/ci<path>` with a body streamed from `rx` (a pack, W17-R06).
    pub(crate) async fn upload(&self, path: &str, rx: async_channel::Receiver<bytes::Bytes>) -> Result<Value> {
        let resp = self.client.request(Method::Post, &format!("{}/api/ci{path}", self.url)).bearer(&self.token).header("content-type", "application/octet-stream").body_stream(rx).timeout(Duration::from_secs(3600)).send().await?;
        let status = resp.status();
        let bytes = resp.bytes(1 << 20).await?;
        let v: Value = serde_json::from_slice(&bytes).unwrap_or(Value::String(String::from_utf8_lossy(&bytes).into_owned()));
        if !(200..300).contains(&status) {
            bail!("{}", v.get("error").and_then(Value::as_str).map(str::to_owned).unwrap_or_else(|| format!("HTTP {status}")));
        }
        Ok(v)
    }

    /// Any route of the API (`path` from `/api`).
    pub(crate) async fn call_api(&self, method: Method, path: &str, body: Option<Value>) -> Result<Value> {
        self.call_for(method, path, body, Duration::from_secs(60)).await
    }

    /// [`Self::call_api`], waiting up to `wait` for the answer.
    pub(crate) async fn call_for(&self, method: Method, path: &str, body: Option<Value>, wait: Duration) -> Result<Value> {
        let mut req = self.client.request(method, &format!("{}{path}", self.url)).bearer(&self.token).timeout(wait);
        if let Some(b) = body {
            req = req.json(&b);
        }
        let resp = req.send().await?;
        let status = resp.status();
        let bytes = resp.bytes(16 << 20).await?;
        let v: Value = if bytes.is_empty() { Value::Null } else { serde_json::from_slice(&bytes).unwrap_or(Value::String(String::from_utf8_lossy(&bytes).into_owned())) };
        if !(200..300).contains(&status) {
            bail!("{}", v.get("error").and_then(Value::as_str).map(str::to_owned).unwrap_or_else(|| format!("HTTP {status}")));
        }
        Ok(v)
    }
}

#[derive(clap::Args)]
pub struct OidcArgs {
    #[arg(long, env = "KILN_URL", global = true)]
    url: Option<String>,
    #[arg(long, env = "KILN_ADMIN_TOKEN", hide_env_values = true, global = true, allow_hyphen_values = true)]
    token: Option<String>,
    #[command(subcommand)]
    cmd: OidcCmd,
}

#[derive(clap::Subcommand)]
enum OidcCmd {
    /// The issuer, its keys and which one signs.
    Status,
    /// The published key set (JWKS), for providers that take keys pasted in (Google Cloud, …).
    Jwks,
    /// Publish the discovery document and keys where providers fetch them (W10-R12), served at
    /// the issuer URL (`[ci.oidc] issuer`): a directory for a static host; `s3://<bucket>[/<prefix>]`,
    /// signed with this machine's AWS_ACCESS_KEY_ID and AWS_SECRET_ACCESS_KEY; or
    /// `gs://<bucket>[/<prefix>]`, with GOOGLE_OAUTH_ACCESS_TOKEN or gcloud's token. Run it again
    /// within 7 days of each new key (`kiln oidc status`), or set `[ci.oidc] publish_to` and the
    /// server keeps them published.
    Publish {
        #[arg(long)]
        to: String,
        /// For s3://: the store's endpoint, AWS S3's in the region by default.
        #[arg(long)]
        endpoint: Option<String>,
        /// For s3://: the region, else AWS_REGION, else us-east-1.
        #[arg(long)]
        region: Option<String>,
    },
    /// Make a new signing key: it signs in 7 days, or at once with `--now`.
    Rotate {
        #[arg(long)]
        now: bool,
    },
    /// The claims ID tokens' subjects are built from (W10-R09, GitHub's "customize subject
    /// claims"), for the server, an organization (`org:<owner>`) or a repository
    /// (`repo:<owner/name>`): shown, set with `--keys repo,context,kiln_trust`, GitHub's default
    /// for a repository with `--default`, or inherited again with `--inherit`.
    Subject {
        #[arg(long, default_value = "server")]
        scope: String,
        #[arg(long, value_delimiter = ',', conflicts_with_all = ["default", "inherit"])]
        keys: Vec<String>,
        #[arg(long, conflicts_with = "inherit")]
        default: bool,
        #[arg(long)]
        inherit: bool,
    },
}

/// `kiln oidc`.
pub async fn oidc(args: OidcArgs, cfg: &Config) -> Result<()> {
    let api = Api::connect(cfg, args.url.as_deref(), args.token.as_deref()).await?;
    match args.cmd {
        OidcCmd::Status => {
            let v = api.call(Method::Get, "/oidc", None).await?;
            println!("issuer {}", v.get("issuer").and_then(Value::as_str).unwrap_or_default());
            for k in v.get("keys").and_then(Value::as_array).cloned().unwrap_or_default() {
                let flag = |n: &str| k.get(n).and_then(Value::as_bool).unwrap_or(false);
                println!("{}  signs from {}{}{}", k.get("kid").and_then(Value::as_str).unwrap_or_default(), when(k.get("sign_from").and_then(Value::as_i64)), if flag("signing") { "  signing" } else { "" }, if flag("published") { "  published" } else { "" });
            }
        }
        OidcCmd::Jwks => {
            let v = api.call(Method::Get, "/oidc/jwks-export", None).await?;
            println!("{}", serde_json::to_string_pretty(v.get("jwks").unwrap_or(&Value::Null))?);
        }
        OidcCmd::Publish { to, endpoint, region } => {
            let v = api.call(Method::Get, "/oidc/jwks-export", None).await?;
            let (Some(discovery), Some(jwks)) = (v.get("discovery"), v.get("jwks")) else { bail!("the server sent no documents") };
            let issuer = v.get("issuer").and_then(Value::as_str).unwrap_or_default();
            match super::oidc::Target::parse(&to)? {
                super::oidc::Target::Dir(dir) => {
                    super::oidc::write_published(&dir, discovery, jwks)?;
                    println!("wrote the discovery document and keys to {}; serve it at {issuer}", dir.display());
                }
                super::oidc::Target::S3 { bucket, prefix } => {
                    let region = region.or_else(|| std::env::var("AWS_REGION").ok()).or_else(|| std::env::var("AWS_DEFAULT_REGION").ok()).unwrap_or_else(|| "us-east-1".into());
                    let base = endpoint.unwrap_or_else(|| super::integrations::aws::endpoint("s3", &region));
                    let creds = super::services::s3::from_env()?;
                    super::oidc::put_s3(&super::integrations::aws::Aws { creds, region }, base.trim_end_matches('/'), &bucket, &prefix, &super::oidc::published_files(discovery, jwks)?).await?;
                    println!("put the discovery document and keys in s3://{bucket}/{prefix}; the issuer must be where they're served (it's {issuer})");
                }
                super::oidc::Target::Gcs { bucket, prefix } => {
                    let token = match std::env::var("GOOGLE_OAUTH_ACCESS_TOKEN").ok().filter(|t| !t.is_empty()) {
                        Some(t) => t,
                        None => {
                            let out = crate::rt::blocking(|| std::process::Command::new("gcloud").args(["auth", "print-access-token"]).output()).await?;
                            let out = out.map_err(|e| anyhow::anyhow!("no GOOGLE_OAUTH_ACCESS_TOKEN, and gcloud didn't run: {e}"))?;
                            anyhow::ensure!(out.status.success(), "no GOOGLE_OAUTH_ACCESS_TOKEN, and gcloud gave no token: {}", String::from_utf8_lossy(&out.stderr).trim());
                            String::from_utf8_lossy(&out.stdout).trim().to_owned()
                        }
                    };
                    super::oidc::put_gcs(&token, &bucket, &prefix, &super::oidc::published_files(discovery, jwks)?).await?;
                    println!("put the discovery document and keys in gs://{bucket}/{prefix}; the issuer must be where they're served (it's {issuer})");
                }
            }
        }
        OidcCmd::Rotate { now } => {
            let v = api.call(Method::Post, "/oidc/rotate", Some(json!({ "now": now }))).await?;
            println!("a new signing key, signing from {}", when(v.get("sign_from").and_then(Value::as_i64)));
        }
        OidcCmd::Subject { scope, keys, default, inherit } => {
            let path = format!("/oidc/subject?scope={}", enc(&scope));
            let v = if inherit {
                api.call(Method::Delete, &path, None).await?
            } else if default || !keys.is_empty() {
                api.call(Method::Put, &path, Some(json!({ "use_default": default, "include_claim_keys": keys }))).await?
            } else {
                api.call(Method::Get, &path, None).await?
            };
            let list = |k: &str| v.get(k).and_then(Value::as_array).map(|a| a.iter().filter_map(Value::as_str).collect::<Vec<_>>().join(", ")).unwrap_or_default();
            let own = if v.get("use_default").and_then(Value::as_bool) == Some(true) { "GitHub's default subject".to_owned() } else { list("include_claim_keys") };
            println!("{}: {}", v.get("scope").and_then(Value::as_str).unwrap_or_default(), if own.is_empty() { "inherited".to_owned() } else { own });
            match v.get("effective_claim_keys").and_then(Value::as_array) {
                Some(_) => println!("subjects are built from: {}", list("effective_claim_keys")),
                None => println!("subjects are GitHub's default: repo:<owner/name>:environment:<env>, :pull_request or :ref:<ref>"),
            }
            if let Some(e) = v.get("example_subject").and_then(Value::as_str) {
                println!("a push to main: {e}");
            }
        }
    }
    Ok(())
}

#[derive(clap::Args)]
pub struct ActionsArgs {
    #[arg(long, env = "KILN_URL", global = true)]
    url: Option<String>,
    #[arg(long, env = "KILN_ADMIN_TOKEN", hide_env_values = true, global = true, allow_hyphen_values = true)]
    token: Option<String>,
    #[command(subcommand)]
    cmd: ActionsCmd,
}

#[derive(clap::Subcommand)]
enum ActionsCmd {
    /// The mirrored actions whose name has QUERY, with their newest versions.
    List { query: Option<String> },
    /// Mirror `[ci.actions] prefetch` now, for a server that will run without reaching GitHub.
    Sync,
    /// The action policy: the server's, and a repository's with `--repo`. `--set` replaces the
    /// repository's with the flags given; `--clear` removes it.
    Policy {
        #[arg(long)]
        repo: Option<String>,
        #[arg(long)]
        set: bool,
        #[arg(long)]
        clear: bool,
        /// `owner/repo[/path][@ref]` globs a run may use (repeatable).
        #[arg(long)]
        allow: Vec<String>,
        #[arg(long)]
        deny: Vec<String>,
        #[arg(long)]
        require_sha_pins: bool,
    },
}

/// `kiln actions`.
pub async fn actions(args: ActionsArgs, cfg: &Config) -> Result<()> {
    let api = Api::connect(cfg, args.url.as_deref(), args.token.as_deref()).await?;
    match args.cmd {
        ActionsCmd::List { query } => {
            let v = api.call(Method::Get, &format!("/actions?q={}", enc(query.as_deref().unwrap_or_default())), None).await?;
            for a in v.get("actions").and_then(Value::as_array).cloned().unwrap_or_default() {
                let versions: Vec<String> = a.get("versions").and_then(Value::as_array).cloned().unwrap_or_default().iter().take(5).filter_map(|x| x.get("ref").and_then(Value::as_str).map(str::to_owned)).collect();
                let about = a.pointer("/action/description").and_then(Value::as_str).unwrap_or_default().lines().next().unwrap_or_default().to_owned();
                println!("{:<44} {:<28} {about}", a.get("uses").and_then(Value::as_str).unwrap_or_default(), versions.join(" "));
            }
        }
        ActionsCmd::Sync => {
            let v = api.call(Method::Post, "/actions/sync", Some(json!({}))).await?;
            let list = v.get("synced").and_then(Value::as_array).cloned().unwrap_or_default();
            if list.is_empty() {
                println!("nothing to mirror: `[ci.actions] prefetch` is empty");
            }
            for s in list {
                let uses = s.get("uses").and_then(Value::as_str).unwrap_or_default();
                match (s.get("sha").and_then(Value::as_str), s.get("error").and_then(Value::as_str)) {
                    (Some(sha), _) => println!("{uses} → {sha}"),
                    (_, e) => println!("{uses}: {}", e.unwrap_or("failed")),
                }
            }
        }
        ActionsCmd::Policy { repo, set, clear, allow, deny, require_sha_pins } => {
            let q = repo.as_deref().map(|r| format!("?repo={}", enc(r))).unwrap_or_default();
            let v = match (set, clear) {
                (true, true) => bail!("--set or --clear, not both"),
                (true, false) if repo.is_some() => api.call(Method::Put, &format!("/actions/policy{q}"), Some(json!({ "allow": allow, "deny": deny, "require_sha_pins": require_sha_pins }))).await?,
                (false, true) if repo.is_some() => api.call(Method::Delete, &format!("/actions/policy{q}"), None).await?,
                (false, false) => api.call(Method::Get, &format!("/actions/policy{q}"), None).await?,
                _ => bail!("the server's policy is `[ci.actions]` in its config; --set and --clear take a --repo"),
            };
            println!("{}", serde_json::to_string_pretty(&v)?);
        }
    }
    Ok(())
}

/// Where the CI API is, for the commands below.
#[derive(clap::Args, Clone)]
pub struct Conn {
    #[arg(long, env = "KILN_URL", global = true)]
    url: Option<String>,
    #[arg(long, env = "KILN_ADMIN_TOKEN", hide_env_values = true, global = true, allow_hyphen_values = true)]
    token: Option<String>,
}

/// Never the token.
impl std::fmt::Debug for Conn {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Conn").field("url", &self.url).field("token", &self.token.as_ref().map(|_| "…")).finish()
    }
}

impl Conn {
    pub(crate) async fn api(&self, cfg: &Config) -> Result<Api> {
        Api::connect(cfg, self.url.as_deref(), self.token.as_deref()).await
    }
}

/// `kiln runs`: the CI engine's runs (docs/ci.md §4.3).
#[derive(clap::Args)]
pub struct RunsArgs {
    #[command(flatten)]
    conn: Conn,
    /// One run, with its legs.
    run: Option<String>,
    #[arg(long)]
    repo: Option<String>,
    #[arg(long)]
    workflow: Option<String>,
    #[arg(long)]
    branch: Option<String>,
    #[arg(long, default_value_t = 20)]
    limit: u32,
    /// Keep watching: a line whenever a run starts or changes.
    #[arg(long)]
    follow: bool,
}

fn run_line(r: &Value) -> String {
    let s = |k: &str| r.get(k).and_then(Value::as_str).unwrap_or_default();
    let state = match s("conclusion") {
        "" => s("status").to_owned(),
        c => c.to_owned(),
    };
    format!("#{:<5} {:<16} {:<28} {:<10} {:<9} {:<8} {}", r.get("number").and_then(Value::as_i64).unwrap_or(0), state, s("workflow").trim_start_matches(".github/workflows/"), s("event"), s("branch"), s("sha").get(..7).unwrap_or_default(), s("id"))
}

pub async fn runs(args: RunsArgs, cfg: &Config) -> Result<()> {
    let api = args.conn.api(cfg).await?;
    if let Some(id) = &args.run {
        let v = api.call(Method::Get, &format!("/runs/{}", enc(id)), None).await?;
        println!("{}", run_line(v.get("run").unwrap_or(&Value::Null)));
        for l in v.get("legs").and_then(Value::as_array).cloned().unwrap_or_default() {
            let s = |k: &str| l.get(k).and_then(Value::as_str).unwrap_or_default().to_owned();
            println!("  {:<32} {:<10} {:<10} {}", if s("key").is_empty() { s("name") } else { s("key") }, s("status"), s("conclusion"), s("id"));
        }
        return Ok(());
    }
    let mut q = format!("/runs?limit={}", args.limit.clamp(1, 200));
    for (k, v) in [("repo", &args.repo), ("workflow", &args.workflow), ("branch", &args.branch)] {
        if let Some(v) = v {
            q.push_str(&format!("&{k}={}", enc(v)));
        }
    }
    let mut seen: std::collections::HashMap<String, String> = std::collections::HashMap::new();
    loop {
        let v = api.call(Method::Get, &q, None).await?;
        let mut list = v.get("runs").and_then(Value::as_array).cloned().unwrap_or_default();
        list.reverse();
        for r in &list {
            let line = run_line(r);
            let id = r.get("id").and_then(Value::as_str).unwrap_or_default().to_owned();
            if seen.get(&id) != Some(&line) {
                println!("{line}");
                seen.insert(id, line);
            }
        }
        if !args.follow {
            return Ok(());
        }
        crate::rt::sleep(Duration::from_secs(2)).await;
    }
}

/// `kiln logs`: a leg's logs, or every leg's of a run.
#[derive(clap::Args)]
pub struct LogsArgs {
    #[command(flatten)]
    conn: Conn,
    /// A run or a leg.
    id: String,
    /// Only this step.
    #[arg(long)]
    step: Option<u32>,
    /// Keep printing until the legs end.
    #[arg(long)]
    follow: bool,
}

pub async fn logs(args: LogsArgs, cfg: &Config) -> Result<()> {
    let api = args.conn.api(cfg).await?;
    let legs: Vec<(String, String)> = match api.call(Method::Get, &format!("/legs/{}", enc(&args.id)), None).await {
        Ok(_) => vec![(args.id.clone(), String::new())],
        Err(_) => {
            let v = api.call(Method::Get, &format!("/runs/{}", enc(&args.id)), None).await?;
            v.get("legs").and_then(Value::as_array).cloned().unwrap_or_default().iter().map(|l| (l.get("id").and_then(Value::as_str).unwrap_or_default().to_owned(), l.get("key").and_then(Value::as_str).unwrap_or_default().to_owned())).collect()
        }
    };
    // What was printed of each leg's steps.
    let mut printed: std::collections::HashMap<(String, u32), usize> = std::collections::HashMap::new();
    loop {
        let mut live = false;
        for (leg, key) in &legs {
            let v = api.call(Method::Get, &format!("/legs/{}", enc(leg)), None).await?;
            let status = v.pointer("/leg/status").or_else(|| v.get("status")).and_then(Value::as_str).unwrap_or_default().to_owned();
            live |= matches!(status.as_str(), "queued" | "pending" | "running" | "assigned");
            let steps: Vec<(u32, String)> = v.get("steps").and_then(Value::as_array).cloned().unwrap_or_default().iter().filter_map(|s| Some((u32::try_from(s.get("idx")?.as_i64()?).ok()?, s.get("name").and_then(Value::as_str).unwrap_or_default().to_owned()))).collect();
            for (idx, name) in steps.iter().filter(|(i, _)| args.step.is_none_or(|n| n == *i)) {
                let text = api.call(Method::Get, &format!("/legs/{}/logs?step={idx}", enc(leg)), None).await?;
                let text = text.as_str().unwrap_or_default().to_owned();
                let at = printed.entry((leg.clone(), *idx)).or_insert(0);
                if text.len() > *at {
                    if *at == 0 {
                        println!("\x1b[1m── {}{name}\x1b[0m", if key.is_empty() { String::new() } else { format!("{key} · ") });
                    }
                    print!("{}", text.get(*at..).unwrap_or_default());
                    *at = text.len();
                }
            }
        }
        if !args.follow || !live {
            return Ok(());
        }
        crate::rt::sleep(Duration::from_secs(1)).await;
    }
}

/// `kiln metrics <tool> <path>` (docs/ci.md W16-R07): a benchmark tool's output as metrics, added
/// to the step's `$KILN_METRICS` (printed when there is none).
#[derive(clap::Args)]
pub struct MetricsArgs {
    /// criterion, hyperfine, gobench, pytest-benchmark, gbench, libtest or k6.
    tool: String,
    /// Its output: a file (`-` for standard input), or criterion's directory (`target/criterion`).
    path: std::path::PathBuf,
    /// `KEY=VALUE` tags on every metric (repeatable).
    #[arg(long = "tag")]
    tags: Vec<String>,
}

pub fn metrics(args: MetricsArgs) -> Result<()> {
    use kiln_ci::results::metrics::{Tool, convert};
    use std::io::Write as _;
    let Some(tool) = Tool::parse(&args.tool) else { bail!("`{}` isn't a tool kiln reads: criterion, hyperfine, gobench, pytest-benchmark, gbench, libtest or k6", args.tool) };
    let mut files: Vec<(String, String)> = Vec::new();
    if args.path.as_os_str() == "-" {
        let mut s = String::new();
        std::io::stdin().read_to_string(&mut s)?;
        files.push(("-".into(), s));
    } else if args.path.is_dir() {
        // criterion: every `…/new/estimates.json` and `sample.json` below.
        let mut stack = vec![args.path.clone()];
        while let Some(dir) = stack.pop() {
            for e in std::fs::read_dir(&dir)?.filter_map(std::result::Result::ok) {
                let p = e.path();
                if p.is_dir() {
                    stack.push(p);
                } else if p.parent().and_then(|d| d.file_name()).is_some_and(|n| n == "new") && p.file_name().is_some_and(|n| n == "estimates.json" || n == "sample.json") {
                    files.push((p.to_string_lossy().replace('\\', "/"), std::fs::read_to_string(&p)?));
                }
            }
        }
    } else {
        files.push((args.path.to_string_lossy().into_owned(), std::fs::read_to_string(&args.path)?));
    }
    let mut points = convert(tool, &files).map_err(|e| anyhow::anyhow!("{}: {e}", args.path.display()))?;
    for t in &args.tags {
        let Some((k, v)) = t.split_once('=') else { bail!("--tag takes KEY=VALUE, not `{t}`") };
        for p in &mut points {
            p.tags.insert(k.to_owned(), v.to_owned());
        }
    }
    let lines: String = points.iter().filter_map(|p| serde_json::to_string(p).ok()).map(|l| l + "\n").collect();
    match std::env::var_os("KILN_METRICS").filter(|v| !v.is_empty()) {
        Some(file) => {
            std::fs::OpenOptions::new().create(true).append(true).open(&file)?.write_all(lines.as_bytes())?;
            eprintln!("{} metrics added to $KILN_METRICS", points.len());
        }
        None => print!("{lines}"),
    }
    Ok(())
}

/// `kiln dispatch`: a `workflow_dispatch` run.
#[derive(clap::Args)]
pub struct DispatchArgs {
    #[command(flatten)]
    conn: Conn,
    /// The workflow: its path or file name.
    workflow: String,
    #[arg(long)]
    repo: String,
    /// The branch or tag (the default branch when omitted).
    #[arg(long = "ref")]
    git_ref: Option<String>,
    /// `name=value`, repeatable.
    #[arg(long = "input")]
    inputs: Vec<String>,
}

pub async fn dispatch(args: DispatchArgs, cfg: &Config) -> Result<()> {
    let api = args.conn.api(cfg).await?;
    let mut inputs = serde_json::Map::new();
    for i in &args.inputs {
        let Some((k, v)) = i.split_once('=') else { bail!("--input takes name=value, not `{i}`") };
        inputs.insert(k.to_owned(), Value::String(v.to_owned()));
    }
    let path = if args.workflow.contains('/') { args.workflow.clone() } else { format!(".github/workflows/{}", args.workflow) };
    let mut body = json!({ "inputs": inputs });
    if let Some(r) = &args.git_ref
        && let Some(o) = body.as_object_mut()
    {
        o.insert("ref".into(), json!(r));
    }
    api.call(Method::Post, &format!("/repos/{}/workflows/{path}/dispatches", enc(&args.repo)), Some(body)).await?;
    println!("dispatched {path} on {}", args.git_ref.as_deref().unwrap_or("the default branch"));
    Ok(())
}

/// `kiln op`: any of kiln's operations (docs/ci.md W20-R01), as the MCP tools and code mode
/// have them: `kiln op` lists them, `kiln op <name> --schema` says what one takes, and
/// `kiln op <name> [<json> | key=value …]` runs it.
#[derive(clap::Args)]
pub struct OpArgs {
    #[command(flatten)]
    conn: Conn,
    /// The operation (`ci.runs.list`); without, every one is listed.
    name: Option<String>,
    /// Its input: a JSON object, or `key=value` pairs (a value that reads as JSON is JSON).
    input: Vec<String>,
    /// What it takes, instead of running it.
    #[arg(long)]
    schema: bool,
}

/// An operation's input from the command line: one JSON object, or `key=value` pairs.
pub fn op_input(args: &[String]) -> Result<Value> {
    if let [one] = args
        && one.trim_start().starts_with('{')
    {
        return serde_json::from_str(one).map_err(|e| anyhow::anyhow!("the input isn't a JSON object: {e}"));
    }
    let mut o = serde_json::Map::new();
    for a in args {
        let Some((k, v)) = a.split_once('=') else { bail!("the input is a JSON object or key=value pairs, not `{a}`") };
        o.insert(k.trim().to_owned(), serde_json::from_str(v).unwrap_or_else(|_| Value::String(v.to_owned())));
    }
    Ok(Value::Object(o))
}

pub async fn op(args: OpArgs, cfg: &Config) -> Result<()> {
    let api = args.conn.api(cfg).await?;
    let Some(name) = args.name else {
        let list = api.call_api(Method::Get, "/api/ops", None).await?;
        for o in list.get("operations").and_then(Value::as_array).into_iter().flatten() {
            let text = |k: &str| o.get(k).and_then(Value::as_str).unwrap_or_default().to_owned();
            let changes = if o.get("read_only").and_then(Value::as_bool) == Some(true) { "" } else { " (changes)" };
            println!("{:<36} {}{changes}", text("name"), text("summary"));
        }
        return Ok(());
    };
    if args.schema {
        let list = api.call_api(Method::Get, "/api/ops", None).await?;
        let o = list.get("operations").and_then(Value::as_array).into_iter().flatten().find(|o| o.get("name").and_then(Value::as_str) == Some(name.as_str())).ok_or_else(|| anyhow::anyhow!("no operation {name}: `kiln op` lists them"))?;
        println!("{}", o.get("summary").and_then(Value::as_str).unwrap_or_default());
        println!("{}", serde_json::to_string_pretty(o.get("input").unwrap_or(&Value::Null))?);
        return Ok(());
    }
    let out = api.call_api(Method::Post, &format!("/api/ops/{}", enc(&name)), Some(op_input(&args.input)?)).await?;
    println!("{}", serde_json::to_string_pretty(&out)?);
    Ok(())
}

/// Runs the registry's operation `name` with `input`, as `kiln op` does (`POST /api/ops/:name`).
async fn run_op(api: &Api, name: &str, input: Value) -> Result<Value> {
    api.call_api(Method::Post, &format!("/api/ops/{}", enc(name)), Some(input)).await
}

/// `kiln integrations`: what jobs and deployments use to reach other systems (W11).
#[derive(clap::Args)]
pub struct IntegrationsArgs {
    #[command(flatten)]
    conn: Conn,
    #[command(subcommand)]
    cmd: IntegrationsCmd,
}

#[derive(clap::Subcommand)]
enum IntegrationsCmd {
    /// Every integration: its name, kind and scope (never its secret).
    List,
    /// Check one works: its credentials, or a federated exchange as a repository's job (and
    /// environment) would make it.
    Verify {
        name: String,
        #[arg(long)]
        repo: Option<String>,
        #[arg(long)]
        environment: Option<String>,
    },
}

pub async fn integrations(args: IntegrationsArgs, cfg: &Config) -> Result<()> {
    let api = args.conn.api(cfg).await?;
    let s = |v: &Value, k: &str| v.get(k).and_then(Value::as_str).unwrap_or_default().to_owned();
    match args.cmd {
        IntegrationsCmd::List => {
            let v = run_op(&api, "ci.integrations.list", json!({})).await?;
            for i in v.get("integrations").and_then(Value::as_array).cloned().unwrap_or_default() {
                let checked = i.get("verified_at").and_then(Value::as_i64).map_or_else(|| "never checked".to_owned(), |t| format!("checked {}", when(Some(t))));
                println!("{:<24} {:<18} {:<28} {checked}", s(&i, "name"), s(&i, "kind"), s(&i, "scope"));
            }
        }
        IntegrationsCmd::Verify { name, repo, environment } => {
            let mut ask = json!({ "integration": name });
            if let Some(o) = ask.as_object_mut() {
                for (k, v) in [("repository", repo), ("environment", environment)] {
                    if let Some(v) = v {
                        o.insert(k.into(), json!(v));
                    }
                }
            }
            let v = run_op(&api, "ci.integrations.verify", ask).await?;
            if v.get("ok").and_then(Value::as_bool) != Some(true) {
                bail!("{name} doesn't work: {}", s(&v, "error"));
            }
            println!("{name} works: {}", s(&v, "message"));
        }
    }
    Ok(())
}

/// `kiln pools`: executor pools (W13-R09).
#[derive(clap::Args)]
pub struct PoolsArgs {
    #[command(flatten)]
    conn: Conn,
    #[command(subcommand)]
    cmd: PoolsCmd,
}

#[derive(clap::Subcommand)]
enum PoolsCmd {
    /// Every pool: its executor, image, labels, and its executors starting, idle and busy.
    List,
    /// Make or change a pool.
    Set {
        name: String,
        /// docker, firecracker, shards or k8s.
        #[arg(long)]
        executor: String,
        #[arg(long)]
        image: Option<String>,
        /// A label it carries (repeatable).
        #[arg(long = "label", required = true)]
        labels: Vec<String>,
        #[arg(long)]
        min_idle: Option<u32>,
        #[arg(long)]
        max: Option<u32>,
    },
    /// Change how many executors a pool keeps idle at least, and has at most.
    Scale {
        name: String,
        #[arg(long)]
        min_idle: Option<u32>,
        #[arg(long)]
        max: Option<u32>,
    },
}

pub async fn pools(args: PoolsArgs, cfg: &Config) -> Result<()> {
    let api = args.conn.api(cfg).await?;
    let s = |v: &Value, k: &str| v.get(k).and_then(Value::as_str).unwrap_or_default().to_owned();
    let n = |v: &Value, k: &str| v.get(k).and_then(Value::as_u64).unwrap_or_default();
    let said = |v: &Value| {
        for w in v.get("warnings").and_then(Value::as_array).cloned().unwrap_or_default() {
            println!("  warning: {}", w.as_str().unwrap_or_default());
        }
    };
    match args.cmd {
        PoolsCmd::List => {
            let v = run_op(&api, "ci.pools.list", json!({})).await?;
            for p in v.get("pools").and_then(Value::as_array).cloned().unwrap_or_default() {
                let labels: Vec<&str> = p.get("labels").and_then(Value::as_array).map(|l| l.iter().filter_map(Value::as_str).collect()).unwrap_or_default();
                println!(
                    "{:<16} {:<12} {:<32} [{}]  {} starting, {} idle, {} busy",
                    s(&p, "name"),
                    s(&p, "executor"),
                    s(&p, "image"),
                    labels.join(", "),
                    n(&p, "starting"),
                    n(&p, "idle"),
                    n(&p, "busy")
                );
            }
        }
        PoolsCmd::Set { name, executor, image, labels, min_idle, max } => {
            let mut spec = json!({ "name": name, "executor": executor, "labels": labels });
            if let Some(o) = spec.as_object_mut() {
                if let Some(i) = image {
                    o.insert("image".into(), json!(i));
                }
                for (k, v) in [("min_idle", min_idle), ("max", max)] {
                    if let Some(v) = v {
                        o.insert(k.into(), json!(v));
                    }
                }
            }
            let v = run_op(&api, "ci.pools.set", spec).await?;
            println!("{name} saved");
            said(&v);
        }
        PoolsCmd::Scale { name, min_idle, max } => {
            if min_idle.is_none() && max.is_none() {
                bail!("say how: --min-idle, --max or both");
            }
            let p = run_op(&api, "ci.pools.get", json!({ "name": name })).await?;
            let mut spec = json!({ "name": name, "executor": s(&p, "executor"), "labels": p.get("labels").cloned().unwrap_or_else(|| json!([])) });
            if let Some(o) = spec.as_object_mut() {
                if let Some(i) = p.get("image").filter(|i| !i.is_null()) {
                    o.insert("image".into(), i.clone());
                }
                o.insert("min_idle".into(), json!(min_idle.map_or_else(|| n(&p, "min_idle"), u64::from)));
                o.insert("max".into(), json!(max.map_or_else(|| n(&p, "max"), u64::from)));
            }
            let v = run_op(&api, "ci.pools.set", spec).await?;
            println!("{name}: heads for {}", v.pointer("/preview/heading_for").and_then(Value::as_u64).map_or_else(|| "its new size".to_owned(), |h| format!("{h} executors")));
            said(&v);
        }
    }
    Ok(())
}

/// `kiln images`: runner images (W13-R11).
#[derive(clap::Args)]
pub struct ImagesArgs {
    #[command(flatten)]
    conn: Conn,
    #[command(subcommand)]
    cmd: ImagesCmd,
}

#[derive(clap::Subcommand)]
enum ImagesCmd {
    /// The images a repository may use: names, platforms, digests, where built.
    List {
        #[arg(long)]
        repo: String,
    },
    /// Check a Dockerfile (or an Agentfile) as the editor does, each finding with its line.
    Validate { file: PathBuf },
    /// The registries kiln pulls through, and what a machine is given to pull through it:
    /// Docker's `registry-mirrors`, and containerd's `hosts.toml` for each, written under
    /// `--containerd` (containerd's `config_path`, `/etc/containerd/certs.d`), naming `--ca` (kiln's
    /// CA, from `/ca.pem`) where kiln's certificate is its own.
    Mirrors {
        #[arg(long)]
        containerd: Option<PathBuf>,
        #[arg(long)]
        ca: Option<PathBuf>,
    },
    /// Build a definition on a runner (`--on`: a label or a runner's name), and with `--push`
    /// push it to kiln's registry and register it (the server owner's only).
    Build {
        name: String,
        /// Its Dockerfile or Agentfile.
        #[arg(long)]
        file: PathBuf,
        #[arg(long)]
        repo: String,
        #[arg(long)]
        on: String,
        /// A platform to build for (repeatable): `linux/amd64`, `linux/arm64`.
        #[arg(long = "platform")]
        platforms: Vec<String>,
        #[arg(long)]
        push: bool,
    },
}

/// A definition's kind, by its file's name.
fn image_kind(file: &std::path::Path) -> &'static str {
    if file.file_name().is_some_and(|n| n.to_string_lossy().eq_ignore_ascii_case("agentfile")) { "agentfile" } else { "dockerfile" }
}

pub async fn images(args: ImagesArgs, cfg: &Config) -> Result<()> {
    let api = args.conn.api(cfg).await?;
    let s = |v: &Value, k: &str| v.get(k).and_then(Value::as_str).unwrap_or_default().to_owned();
    match args.cmd {
        ImagesCmd::List { repo } => {
            let v = run_op(&api, "ci.images.list", json!({ "repo": repo })).await?;
            for i in v.get("images").and_then(Value::as_array).cloned().unwrap_or_default() {
                let platforms: Vec<&str> = i.get("platforms").and_then(Value::as_array).map(|l| l.iter().filter_map(Value::as_str).collect()).unwrap_or_default();
                println!("{:<20} {:<28} {}  {}", s(&i, "name"), platforms.join(", "), s(&i, "image"), when(i.get("created_at").and_then(Value::as_i64)));
            }
        }
        ImagesCmd::Validate { file } => {
            let content = std::fs::read_to_string(&file).with_context(|| format!("reading {}", file.display()))?;
            let v = run_op(&api, "ci.images.validate", json!({ "content": content, "path": file.to_string_lossy(), "kind": image_kind(&file) })).await?;
            let found = v.get("diagnostics").and_then(Value::as_array).cloned().unwrap_or_default();
            let mut errors = 0;
            for d in &found {
                let severity = s(d, "severity");
                errors += usize::from(severity == "error");
                println!("{}:{}: {} {}: {}", file.display(), d.pointer("/range/start/line").and_then(Value::as_u64).unwrap_or(1), severity, s(d, "code"), s(d, "message"));
            }
            if let Some(note) = v.get("note").and_then(Value::as_str) {
                println!("{}: {note}", file.display());
            }
            if errors > 0 {
                bail!("{errors} errors");
            }
            if found.is_empty() {
                println!("{}: no findings", file.display());
            }
        }
        ImagesCmd::Mirrors { containerd, ca } => {
            let v = run_op(&api, "ci.registry.mirrors", json!({})).await?;
            let list = v.get("mirrors").and_then(Value::as_array).cloned().unwrap_or_default();
            if list.is_empty() {
                println!("kiln pulls through no registry: [[ci.registry.mirrors]] in the server's kiln.toml names them");
                return Ok(());
            }
            for m in &list {
                let anonymous = m.get("anonymous").and_then(Value::as_bool).unwrap_or(false);
                println!("{:<24} /v2/{}/…  {}", s(m, "upstream"), s(m, "prefix"), if anonymous { "anyone pulls" } else { "pulls signed in" });
            }
            if let Some(d) = v.get("docker").filter(|d| d.get("registry-mirrors").and_then(Value::as_array).is_some_and(|l| !l.is_empty())) {
                println!("\nDocker (daemon.json):\n{}", serde_json::to_string_pretty(d)?);
            }
            for note in v.get("notes").and_then(Value::as_array).into_iter().flatten().filter_map(Value::as_str) {
                println!("note: {note}");
            }
            let files = v.get("containerd").and_then(Value::as_object).cloned().unwrap_or_default();
            match containerd {
                Some(dir) => {
                    for (path, text) in &files {
                        let rel = path.strip_prefix("/etc/containerd/certs.d/").unwrap_or(path).trim_start_matches('/');
                        anyhow::ensure!(!rel.split('/').any(|p| p == ".." || p.is_empty()), "`{path}` isn't a path under the folder");
                        let mut text = text.as_str().unwrap_or_default().to_owned();
                        if let Some(ca) = &ca {
                            text = text.replace("  override_path = true\n", &format!("  override_path = true\n  ca = {}\n", serde_json::to_string(&ca.to_string_lossy())?));
                        }
                        let to = dir.join(rel);
                        if let Some(parent) = to.parent() {
                            std::fs::create_dir_all(parent).with_context(|| format!("making {}", parent.display()))?;
                        }
                        std::fs::write(&to, text).with_context(|| format!("writing {}", to.display()))?;
                        println!("wrote {}", to.display());
                    }
                    println!("containerd reads them when its registry `config_path` is {}", dir.display());
                }
                None => {
                    for (path, text) in &files {
                        println!("\n# {path}\n{}", text.as_str().unwrap_or_default());
                    }
                }
            }
        }
        ImagesCmd::Build { name, file, repo, on, platforms, push } => {
            let content = std::fs::read_to_string(&file).with_context(|| format!("reading {}", file.display()))?;
            let v = run_op(&api, "ci.images.build", json!({ "repo": repo, "name": name, "kind": image_kind(&file), "content": content, "runs_on": on, "platforms": platforms, "push": push })).await?;
            let run = s(&v, "run");
            println!("building {name} on {on}{}: `kiln logs {run} --follow` follows it", if push { " (then pushed)" } else { "" });
        }
    }
    Ok(())
}

/// `kiln experiment`: options measured against each other and together (W20-R11).
#[derive(clap::Args)]
pub struct ExperimentArgs {
    #[command(flatten)]
    conn: Conn,
    #[command(subcommand)]
    cmd: ExperimentCmd,
}

#[derive(clap::Subcommand)]
enum ExperimentCmd {
    /// Start one: from a file (`--spec`, what `experiments.start` takes, JSON or YAML), or from
    /// these flags: each option a name and a branch, a commit or a diff (`--option fast=opt-fast`,
    /// `--option cache=D12`), the workflow that measures them (`--pipeline`, required to pass),
    /// and the metric to better (`--metric p99_ms`, lower is better; `p99_ms:higher`).
    Start {
        #[arg(long)]
        spec: Option<PathBuf>,
        #[arg(long)]
        repo: Option<String>,
        #[arg(long = "option")]
        options: Vec<String>,
        /// each, all (every combination), pairs, greedy.
        #[arg(long, default_value = "all")]
        strategy: String,
        #[arg(long)]
        pipeline: Option<String>,
        #[arg(long)]
        metric: Option<String>,
        #[arg(long, default_value_t = 3)]
        repetitions: usize,
        #[arg(long, default_value_t = 4)]
        parallel: usize,
        /// Take the clear winner instead of bringing the results back.
        #[arg(long)]
        auto: bool,
        #[arg(long)]
        question: Option<String>,
    },
    /// Experiments, newest first.
    List {
        #[arg(long)]
        repo: Option<String>,
    },
    /// One: its variants, what each measured against the baseline, and the recommendation.
    Get { experiment: String },
    /// Continue with a variant.
    Decide { experiment: String, variant: String },
    /// Stop one: its runs are cancelled.
    Cancel { experiment: String },
}

/// An option from the command line: `name=<branch | 40-hex commit | D12>`.
fn option_of(text: &str) -> Result<Value> {
    let (name, at) = text.split_once('=').ok_or_else(|| anyhow::anyhow!("`{text}`: an option is name=<branch, commit or diff>"))?;
    let at = at.trim();
    let kind = if at.len() == 40 && at.bytes().all(|b| b.is_ascii_hexdigit()) {
        "commit"
    } else if at.len() > 1 && at.starts_with('D') && at.get(1..).is_some_and(|n| n.bytes().all(|b| b.is_ascii_digit())) {
        "diff"
    } else {
        "branch"
    };
    Ok(json!({ "name": name.trim(), kind: at }))
}

pub async fn experiment(args: ExperimentArgs, cfg: &Config) -> Result<()> {
    let api = args.conn.api(cfg).await?;
    let s = |v: &Value, k: &str| v.get(k).and_then(Value::as_str).unwrap_or_default().to_owned();
    match args.cmd {
        ExperimentCmd::Start { spec, repo, options, strategy, pipeline, metric, repetitions, parallel, auto, question } => {
            let ask = match spec {
                Some(file) => {
                    let text = std::fs::read_to_string(&file).with_context(|| format!("reading {}", file.display()))?;
                    serde_yaml::from_str::<Value>(&text).with_context(|| format!("{} isn't JSON or YAML", file.display()))?
                }
                None => {
                    let repo = repo.ok_or_else(|| anyhow::anyhow!("say which repository (`--repo`), or give a `--spec`"))?;
                    let pipeline = pipeline.ok_or_else(|| anyhow::anyhow!("say which workflow measures the variants (`--pipeline .kiln/bench.yaml`)"))?;
                    let mut criteria = vec![json!({ "name": pipeline.rsplit('/').next().unwrap_or(&pipeline), "kind": "pipeline", "workflow": pipeline, "required": true })];
                    if let Some(m) = metric {
                        let (name, better) = m.split_once(':').unwrap_or((m.as_str(), "lower"));
                        criteria.push(json!({ "name": name, "kind": "metric", "metric": name, "better": better }));
                    }
                    let opts: Vec<Value> = options.iter().map(|o| option_of(o)).collect::<Result<_>>()?;
                    json!({ "repo": repo, "options": opts, "strategy": strategy, "criteria": criteria, "repetitions": repetitions, "max_parallel": parallel, "decide": if auto { "auto" } else { "ask" }, "question": question })
                }
            };
            let v = run_op(&api, "experiments.start", ask).await?;
            let e = v.get("experiment").cloned().unwrap_or_default();
            let names: Vec<String> = e.get("variants").and_then(Value::as_array).into_iter().flatten().map(|x| s(x, "name")).collect();
            println!("{} started on {}: {} variants ({}); `kiln experiment get {}` follows it", s(&e, "id"), s(&e, "repo"), names.len(), names.join(", "), s(&e, "id"));
        }
        ExperimentCmd::List { repo } => {
            let v = run_op(&api, "experiments.list", repo.map_or_else(|| json!({}), |r| json!({ "repo": r }))).await?;
            for e in v.get("experiments").and_then(Value::as_array).cloned().unwrap_or_default() {
                let n = |k: &str| e.get(k).and_then(Value::as_u64).unwrap_or_default();
                let rec = e.get("recommendation").and_then(Value::as_str).map(|r| format!("  recommends {r}")).unwrap_or_default();
                println!("{:<6} {:<20} {:<10} {}/{} runs{rec}  {}", s(&e, "id"), s(&e, "repo"), s(&e, "state"), n("measured"), n("runs"), s(&e, "question"));
            }
        }
        ExperimentCmd::Get { experiment } => {
            let v = run_op(&api, "experiments.get", json!({ "experiment": experiment })).await?;
            let e = v.get("experiment").cloned().unwrap_or_default();
            println!("{} on {} ({}): {}", s(&e, "id"), s(&e, "repo"), s(&e, "state"), s(&e, "question"));
            let results = e.get("results").cloned().unwrap_or_default();
            for b in e.get("variants").and_then(Value::as_array).cloned().unwrap_or_default() {
                let name = s(&b, "name");
                let r = results.get("variants").and_then(Value::as_array).and_then(|vs| vs.iter().find(|x| x.get("name").and_then(Value::as_str) == Some(name.as_str())).cloned()).unwrap_or_default();
                let checks = if r.get("passes").and_then(Value::as_bool) == Some(false) {
                    "fails a required check"
                } else if r.is_null() {
                    ""
                } else {
                    "passes"
                };
                let metrics: Vec<String> = r
                    .get("metrics")
                    .and_then(Value::as_array)
                    .into_iter()
                    .flatten()
                    .map(|m| match m.get("vs_baseline") {
                        Some(i) if !i.is_null() => {
                            let f = |k: &str| i.get(k).and_then(Value::as_f64).unwrap_or_default();
                            format!("{} ×{:.3} (95% {:.3} to {:.3})", s(m, "name"), f("mid"), f("low"), f("high"))
                        }
                        _ => format!("{} {}", s(m, "name"), m.get("median").and_then(Value::as_f64).map(|x| format!("{x}")).unwrap_or_default()),
                    })
                    .collect();
                let state = if s(&b, "state") == "conflict" { format!("conflict: {}", s(&b, "why")) } else { checks.to_owned() };
                println!("  {name:<16} {state:<24} {}", metrics.join("; "));
            }
            if let Some(why) = results.get("why").and_then(Value::as_str) {
                println!("{why}");
            }
            if let Some(d) = e.get("decision").filter(|d| !d.is_null()) {
                println!("decided: {} ({}) by {}", s(d, "variant"), s(d, "commit").get(..12).unwrap_or_default(), s(d, "by"));
            }
        }
        ExperimentCmd::Decide { experiment, variant } => {
            let v = run_op(&api, "experiments.decide", json!({ "experiment": experiment, "variant": variant })).await?;
            println!("{experiment}: continuing with {variant} ({})", v.pointer("/experiment/decision/commit").and_then(Value::as_str).unwrap_or_default());
        }
        ExperimentCmd::Cancel { experiment } => {
            run_op(&api, "experiments.cancel", json!({ "experiment": experiment })).await?;
            println!("{experiment} cancelled");
        }
    }
    Ok(())
}

/// `kiln store`: the server's content-addressed store (W08-R01).
#[derive(clap::Args)]
pub struct StoreArgs {
    #[command(flatten)]
    conn: Conn,
    #[command(subcommand)]
    cmd: StoreCmd,
}

#[derive(clap::Subcommand)]
enum StoreCmd {
    /// Re-hash every chunk on the server's disk; any whose contents aren't their digest is moved
    /// aside (exits 1 when one is).
    Verify,
    /// Collect now: objects nothing refers to, then chunks no object uses.
    Gc,
}

pub async fn store(args: StoreArgs, cfg: &Config) -> Result<()> {
    let api = args.conn.api(cfg).await?;
    let n = |v: &Value, k: &str| v.get(k).and_then(Value::as_u64).unwrap_or_default();
    match args.cmd {
        StoreCmd::Verify => {
            let v = run_op(&api, "ci.store.verify", json!({})).await?;
            let bad: Vec<&str> = v.get("corrupt").and_then(Value::as_array).map(|c| c.iter().filter_map(Value::as_str).collect()).unwrap_or_default();
            println!("{} chunks, {} bytes", n(&v, "chunks"), n(&v, "bytes"));
            for d in &bad {
                println!("corrupt: {d} (moved aside)");
            }
            if !bad.is_empty() {
                bail!("{} chunks weren't what their digest says", bad.len());
            }
        }
        StoreCmd::Gc => {
            let v = run_op(&api, "ci.store.gc", json!({})).await?;
            println!("removed {} objects ({} bytes)", n(&v, "objects"), n(&v, "bytes"));
        }
    }
    Ok(())
}

/// `kiln runners facts|probe <name>`: a runner's facts (W13-R01), as it last reported them or
/// probed again now.
#[derive(clap::Subcommand, Clone, Debug)]
pub enum RunnerFacts {
    /// A runner's facts, as it last reported them.
    Facts {
        name: String,
        #[command(flatten)]
        conn: Conn,
    },
    /// Have a runner probe its facts again now, and show them.
    Probe {
        name: String,
        #[command(flatten)]
        conn: Conn,
    },
}

pub async fn runner_facts(cmd: RunnerFacts, cfg: &Config) -> Result<()> {
    let facts = match cmd {
        RunnerFacts::Facts { name, conn } => {
            let api = conn.api(cfg).await?;
            let all = run_op(&api, "ci.runners.list", json!({})).await?;
            let found = all.as_array().into_iter().flatten().find(|r| r.get("name").and_then(Value::as_str) == Some(name.as_str())).cloned().ok_or_else(|| anyhow::anyhow!("no runner `{name}`"))?;
            found.get("facts").cloned().filter(|f| !f.is_null()).ok_or_else(|| anyhow::anyhow!("{name} hasn't reported its facts: `kiln runners probe {name}` asks"))?
        }
        RunnerFacts::Probe { name, conn } => {
            let api = conn.api(cfg).await?;
            let v = run_op(&api, "ci.runners.probe", json!({ "runner": name })).await?;
            v.get("facts").cloned().filter(|f| !f.is_null()).ok_or_else(|| anyhow::anyhow!("{name} was asked; its report hasn't come yet"))?
        }
    };
    println!("{}", serde_json::to_string_pretty(&facts)?);
    Ok(())
}

/// `kiln rerun`: a new attempt of a run (W05-R17).
#[derive(clap::Args)]
pub struct RerunArgs {
    #[command(flatten)]
    conn: Conn,
    run: String,
    /// Only the failed jobs, and the jobs that need them.
    #[arg(long, conflicts_with = "job")]
    failed: bool,
    /// Only this job, and the jobs that need it.
    #[arg(long)]
    job: Option<String>,
    /// With debug logging (`ACTIONS_STEP_DEBUG`, `ACTIONS_RUNNER_DEBUG`).
    #[arg(long)]
    debug: bool,
    /// Resolve the actions' refs again instead of reusing the run's pins.
    #[arg(long)]
    fresh_actions: bool,
    /// Keep a leg that fails this many minutes to look into (terminal, `kiln debug`).
    #[arg(long, value_name = "MINUTES", num_args = 0..=1, default_missing_value = "30")]
    keep_failed: Option<u32>,
    /// Run every leg, even those whose inputs match an earlier result they could reuse
    /// (`reuse:`, docs/ci.md W08-R10).
    #[arg(long)]
    no_reuse: bool,
    /// Run cold (docs/ci.md W24-R23): no leg reuses an earlier result, starts from kept folders or
    /// restores a cache.
    #[arg(long)]
    cold: bool,
}

pub async fn rerun(args: RerunArgs, cfg: &Config) -> Result<()> {
    let api = args.conn.api(cfg).await?;
    let mode = if args.failed { "failed" } else if args.job.is_some() { "job" } else { "all" };
    let body = json!({ "mode": mode, "job": args.job, "debug": args.debug, "fresh_actions": args.fresh_actions, "keep_failed": args.keep_failed, "no_reuse": args.no_reuse, "cold": args.cold });
    let v = api.call(Method::Post, &format!("/runs/{}/rerun", enc(&args.run)), Some(body)).await?;
    match v.get("attempt").and_then(Value::as_i64) {
        Some(a) => println!("attempt {a} of run {} started", args.run),
        None => println!("rerun of {} started", args.run),
    }
    Ok(())
}

/// `kiln cancel`.
#[derive(clap::Args)]
pub struct CancelArgs {
    #[command(flatten)]
    conn: Conn,
    run: String,
}

pub async fn cancel(args: CancelArgs, cfg: &Config) -> Result<()> {
    let api = args.conn.api(cfg).await?;
    api.call(Method::Post, &format!("/runs/{}/cancel", enc(&args.run)), Some(json!({}))).await?;
    println!("cancelling {}", args.run);
    Ok(())
}

/// `kiln routing explain <leg>`, or for a workflow's jobs before they run.
#[derive(clap::Args)]
pub struct RoutingArgs {
    #[command(flatten)]
    conn: Conn,
    #[command(subcommand)]
    cmd: RoutingCmd,
}

#[derive(clap::Subcommand)]
enum RoutingCmd {
    /// Why a leg waits and how every runner measures up; or, with --repo and a workflow path,
    /// where its legs would go now.
    Explain {
        /// A leg id, or a workflow path with --repo.
        target: String,
        #[arg(long)]
        repo: Option<String>,
        #[arg(long = "ref")]
        git_ref: Option<String>,
    },
    /// Where a leg with these `runs-on` labels would go now, by the rules and the runners.
    Where {
        #[arg(required = true)]
        labels: Vec<String>,
        #[arg(long)]
        repo: Option<String>,
        /// Constraints of its own (`requires:`), e.g. `"mem.total >= 16Gi"`.
        #[arg(long = "require")]
        requires: Vec<String>,
    },
    /// The routing rules in the order they apply, with where each comes from; `--set FILE`
    /// replaces a scope's stored rules with the YAML list in FILE, `--clear` removes them.
    Rules {
        /// `server` or `org:<owner>` (a repository's rules are its `.kiln/routing.yaml`).
        #[arg(long, default_value = "server")]
        scope: String,
        #[arg(long)]
        set: Option<std::path::PathBuf>,
        #[arg(long)]
        clear: bool,
    },
}

fn print_runners(runners: &[Value]) {
    for r in runners {
        let failed: Vec<String> = r.get("checks").and_then(Value::as_array).cloned().unwrap_or_default().iter().filter(|c| !c.get("pass").and_then(Value::as_bool).unwrap_or(true)).map(|c| format!("{} (has {})", c.get("requirement").and_then(Value::as_str).unwrap_or("a check"), c.get("value").and_then(Value::as_str).unwrap_or("nothing"))).collect();
        let flag = |k: &str| r.get(k).and_then(Value::as_bool).unwrap_or(false);
        println!("    {:<20} {}{}{}", r.get("name").and_then(Value::as_str).unwrap_or_default(), if flag("eligible") { "eligible" } else { "not eligible" }, if flag("eligible") && !flag("room") { ", busy" } else { "" }, if failed.is_empty() { String::new() } else { format!(": {}", failed.join("; ")) });
    }
}

pub async fn routing(args: RoutingArgs, cfg: &Config) -> Result<()> {
    let api = args.conn.api(cfg).await?;
    let (target, repo, git_ref) = match args.cmd {
        RoutingCmd::Explain { target, repo, git_ref } => (target, repo, git_ref),
        RoutingCmd::Where { labels, repo, requires } => {
            let v = api.call(Method::Post, "/routing/explain", Some(json!({ "labels": labels, "repo": repo.unwrap_or_default(), "requires": requires }))).await?;
            let strs = |k: &str| v.get(k).and_then(Value::as_array).cloned().unwrap_or_default().iter().filter_map(|x| x.as_str().map(str::to_owned)).collect::<Vec<_>>().join(", ");
            println!("labels left for runners: [{}]; constraints: [{}]; executors: [{}]; image: {}", strs("labels"), strs("constraints"), strs("executors"), v.get("image").and_then(Value::as_str).unwrap_or("none"));
            for r in v.get("rules").and_then(Value::as_array).cloned().unwrap_or_default() {
                println!("  rule from {}: {}", r.get("source").and_then(Value::as_str).unwrap_or_default(), r.get("rule").map(Value::to_string).unwrap_or_default());
            }
            print_runners(&v.get("runners").and_then(Value::as_array).cloned().unwrap_or_default());
            return Ok(());
        }
        RoutingCmd::Rules { scope, set, clear } => {
            if set.is_some() || clear {
                let rules: Value = match set {
                    Some(f) => serde_yaml::from_str(&std::fs::read_to_string(&f)?)?,
                    None => json!([]),
                };
                let rules = rules.get("rules").cloned().unwrap_or(rules);
                api.call(Method::Put, &format!("/routing/rules?scope={}", enc(&scope)), Some(json!({ "rules": rules }))).await?;
            }
            let v = api.call(Method::Get, "/routing/rules", None).await?;
            for r in v.get("rules").and_then(Value::as_array).cloned().unwrap_or_default() {
                println!("{:>3}  {:<24} {}", r.get("index").and_then(Value::as_u64).unwrap_or_default(), r.get("source").and_then(Value::as_str).unwrap_or_default(), r.get("rule").map(Value::to_string).unwrap_or_default());
            }
            return Ok(());
        }
    };
    let Some(repo) = repo else {
        let v = api.call(Method::Get, &format!("/legs/{}/routing", enc(&target)), None).await?;
        if let Some(w) = v.get("wait_reason").filter(|w| !w.is_null()) {
            println!("waiting: {w}");
        }
        print_runners(&v.get("runners").and_then(Value::as_array).cloned().unwrap_or_default());
        return Ok(());
    };
    let mut body = json!({ "repo": repo, "path": target, "event": "push" });
    if let Some(r) = git_ref
        && let Some(o) = body.as_object_mut()
    {
        o.insert("ref".into(), json!(r));
    }
    let v = api.call(Method::Post, "/plan-preview", Some(body)).await?;
    for l in v.pointer("/plan/legs").and_then(Value::as_array).cloned().unwrap_or_default() {
        let labels: Vec<String> = l.pointer("/routing/labels").and_then(Value::as_array).cloned().unwrap_or_default().iter().filter_map(|x| x.as_str().map(str::to_owned)).collect();
        println!("{}  (labels: {}; image: {})", l.get("key").and_then(Value::as_str).unwrap_or_default(), labels.join(", "), l.pointer("/routing/image").and_then(Value::as_str).unwrap_or("none"));
        print_runners(&l.pointer("/routing/runners").and_then(Value::as_array).cloned().unwrap_or_default());
    }
    Ok(())
}

/// Percent-encodes a query value.
pub(crate) fn enc(s: &str) -> String {
    s.bytes().map(|b| if b.is_ascii_alphanumeric() || b"-._~".contains(&b) { (b as char).to_string() } else { format!("%{b:02X}") }).collect()
}

pub(crate) fn when(t: Option<i64>) -> String {
    t.and_then(|t| time::OffsetDateTime::from_unix_timestamp(t).ok()).and_then(|t| t.format(&time::format_description::well_known::Rfc3339).ok()).unwrap_or_else(|| "never".into())
}

/// `NAME=value` lines of a dotenv file (comments, blank lines and `export` skipped; quotes removed).
pub(crate) fn dotenv(text: &str) -> Vec<(String, String)> {
    text.lines()
        .filter_map(|l| {
            let l = l.trim();
            if l.is_empty() || l.starts_with('#') {
                return None;
            }
            let l = l.strip_prefix("export ").unwrap_or(l);
            let (k, v) = l.split_once('=')?;
            let v = v.trim();
            let v = v.strip_prefix('"').and_then(|x| x.strip_suffix('"')).or_else(|| v.strip_prefix('\'').and_then(|x| x.strip_suffix('\''))).unwrap_or(v);
            Some((k.trim().to_owned(), v.to_owned()))
        })
        .collect()
}

/// `kiln secrets`.
pub async fn secrets(args: SecretsArgs, cfg: &Config) -> Result<()> {
    let api = Api::connect(cfg, args.url.as_deref(), args.token.as_deref()).await?;
    match args.cmd {
        SecretsCmd::List { scope } => {
            let v = api.call(Method::Get, &format!("/secrets?scope={}", enc(&scope)), None).await?;
            for s in v.get("secrets").and_then(Value::as_array).cloned().unwrap_or_default() {
                let name = s.get("name").and_then(Value::as_str).unwrap_or_default();
                let size = s.get("size").and_then(Value::as_i64).unwrap_or(0);
                println!("{name:<32} {size:>6} B  updated {}  used {}", when(s.get("updated_at").and_then(Value::as_i64)), when(s.get("last_used_at").and_then(Value::as_i64)));
            }
        }
        SecretsCmd::Set { name, scope, from: Some(from), .. } => {
            api.call(Method::Put, &format!("/secrets/{}?scope={}", enc(&name), enc(&scope)), Some(json!({ "from": from }))).await?;
            println!("set {} in {scope}, from {from}", name.to_ascii_uppercase());
        }
        SecretsCmd::Set { name, scope, from_file, from: None } => {
            let value = match from_file {
                Some(p) => std::fs::read_to_string(&p)?,
                None => {
                    let mut s = String::new();
                    std::io::stdin().read_to_string(&mut s)?;
                    s.strip_suffix('\n').map(|x| x.strip_suffix('\r').unwrap_or(x)).unwrap_or(&s).to_owned()
                }
            };
            api.call(Method::Put, &format!("/secrets/{}?scope={}", enc(&name), enc(&scope)), Some(json!({ "value": value }))).await?;
            println!("set {} in {scope}", name.to_ascii_uppercase());
        }
        SecretsCmd::Delete { name, scope } => {
            api.call(Method::Delete, &format!("/secrets/{}?scope={}", enc(&name), enc(&scope)), None).await?;
            println!("deleted {} from {scope}", name.to_ascii_uppercase());
        }
        SecretsCmd::Import { dotenv: path, scope } => {
            let pairs = dotenv(&std::fs::read_to_string(&path)?);
            for (k, v) in &pairs {
                api.call(Method::Put, &format!("/secrets/{}?scope={}", enc(k), enc(&scope)), Some(json!({ "value": v }))).await?;
            }
            println!("set {} secrets in {scope}", pairs.len());
        }
        SecretsCmd::RotateKey => {
            let v = api.call(Method::Post, "/secrets/rotate", None).await?;
            println!("re-sealed {} secrets under a new data key", v.get("resealed").and_then(Value::as_u64).unwrap_or(0));
        }
    }
    Ok(())
}

/// `kiln vars`.
pub async fn vars(args: VarsArgs, cfg: &Config) -> Result<()> {
    let api = Api::connect(cfg, args.url.as_deref(), args.token.as_deref()).await?;
    match args.cmd {
        VarsCmd::List { scope } => {
            let v = api.call(Method::Get, &format!("/variables?scope={}", enc(&scope)), None).await?;
            for s in v.get("variables").and_then(Value::as_array).cloned().unwrap_or_default() {
                println!("{}={}", s.get("name").and_then(Value::as_str).unwrap_or_default(), s.get("value").and_then(Value::as_str).unwrap_or_default());
            }
        }
        VarsCmd::Set { name, value, scope } => {
            api.call(Method::Put, &format!("/variables/{}?scope={}", enc(&name), enc(&scope)), Some(json!({ "value": value }))).await?;
            println!("set {} in {scope}", name.to_ascii_uppercase());
        }
        VarsCmd::Delete { name, scope } => {
            api.call(Method::Delete, &format!("/variables/{}?scope={}", enc(&name), enc(&scope)), None).await?;
            println!("deleted {} from {scope}", name.to_ascii_uppercase());
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dotenv_files() {
        let got = dotenv("# c\n\nexport A=1\nB=\"two words\"\nC='x=y'\nbad line\n");
        assert_eq!(got, vec![("A".into(), "1".into()), ("B".into(), "two words".into()), ("C".into(), "x=y".into())]);
        assert_eq!(enc("repo:acme/app"), "repo%3Aacme%2Fapp");
    }
}

#[derive(clap::Args)]
pub struct EnvArgs {
    #[command(flatten)]
    conn: Conn,
    #[command(subcommand)]
    cmd: EnvCmd,
}

/// `kiln env create`'s arguments.
#[derive(clap::Args)]
struct EnvCreate {
    name: String,
    #[arg(long)]
    repo: String,
    /// A required reviewer: a person, or `team:<name>` (repeatable).
    #[arg(long = "reviewer")]
    reviewers: Vec<String>,
    #[arg(long)]
    prevent_self_review: bool,
    /// Minutes a job waits before it deploys.
    #[arg(long, default_value_t = 0)]
    wait_timer: u32,
    /// Branches that may deploy (patterns, repeatable); with none and no `--tag`, any ref.
    #[arg(long = "branch")]
    branches: Vec<String>,
    /// Tags that may deploy (patterns, repeatable).
    #[arg(long = "tag")]
    tags: Vec<String>,
    #[arg(long)]
    url: Option<String>,
    /// At most this many deployments at once; the rest wait their turn.
    #[arg(long)]
    concurrency: Option<u32>,
    /// What it deploys to: kubernetes, host, container, microvm or platform.
    #[arg(long)]
    kind: Option<String>,
    /// A kiln CD cluster it deploys to (kubernetes; repeatable).
    #[arg(long = "cluster")]
    clusters: Vec<String>,
    /// The namespace it deploys to (kubernetes).
    #[arg(long)]
    namespace: Option<String>,
    /// The runners it deploys on, as a constraint they meet (host, container, microvm):
    /// `label deploy-web`, `os == linux && label web`.
    #[arg(long)]
    runners: Option<String>,
    /// The integration it deploys through (platform).
    #[arg(long)]
    integration: Option<String>,
}

#[derive(clap::Subcommand)]
enum EnvCmd {
    /// A repository's environments, their protection and latest deployment.
    List {
        #[arg(long)]
        repo: String,
    },
    /// Make an environment in kiln (or change one made here), with its protection rules.
    Create(Box<EnvCreate>),
    /// Make a repository's starter environments in one step: dev, staging (the trunk only) and
    /// production (the trunk only, once you approve), each unless it has one of that name.
    Starter {
        #[arg(long)]
        repo: String,
    },
    /// Remove an environment made in kiln.
    Delete {
        name: String,
        #[arg(long)]
        repo: String,
    },
    /// An environment's deployments, newest first.
    Deployments {
        name: String,
        #[arg(long)]
        repo: String,
    },
    /// Deploy there by hand: the workflow that does (one with `workflow_dispatch` and a job bound
    /// to the environment), run at `--ref` (the trunk by default). Its job waits for the
    /// environment's protection as any deployment's does.
    Deploy {
        name: String,
        #[arg(long)]
        repo: String,
        #[arg(long = "ref")]
        git_ref: Option<String>,
        /// The workflow, when several deploy there.
        #[arg(long)]
        workflow: Option<String>,
        /// An input, `key=value` (repeatable).
        #[arg(long = "input")]
        inputs: Vec<String>,
    },
    /// Approve (or reject) a run's deployments waiting for review, as `kiln approve` does.
    Approve {
        run: String,
        /// The environments decided (default: every one the run waits for; repeatable).
        #[arg(long = "env")]
        environments: Vec<String>,
        #[arg(long)]
        reject: bool,
        #[arg(long, default_value = "")]
        comment: String,
    },
}

/// `kiln env`.
pub async fn env(args: EnvArgs, cfg: &Config) -> Result<()> {
    let api = args.conn.api(cfg).await?;
    let s = |v: &Value, k: &str| v.get(k).and_then(Value::as_str).unwrap_or_default().to_owned();
    match args.cmd {
        EnvCmd::List { repo } => {
            let v = api.call(Method::Get, &format!("/environments?repo={}", enc(&repo)), None).await?;
            for e in v.get("environments").and_then(Value::as_array).cloned().unwrap_or_default() {
                let p = e.get("protection").cloned().unwrap_or_default();
                let mut rules = Vec::new();
                if let Some(r) = p.get("reviewers").and_then(Value::as_array).filter(|r| !r.is_empty()) {
                    rules.push(format!("reviewers {}", r.iter().filter_map(Value::as_str).collect::<Vec<_>>().join(", ")));
                }
                if let Some(w) = p.get("wait-timer").and_then(Value::as_u64).filter(|w| *w > 0) {
                    rules.push(format!("waits {w}m"));
                }
                for k in ["branches", "tags"] {
                    if let Some(b) = p.get(k).and_then(Value::as_array) {
                        rules.push(format!("{k} {}", b.iter().filter_map(Value::as_str).collect::<Vec<_>>().join(", ")));
                    }
                }
                if let Some(n) = e.get("concurrency").and_then(Value::as_u64) {
                    rules.push(format!("{n} at once"));
                }
                let last = e.get("latest_deployment").filter(|d| !d.is_null()).map(|d| format!("; last: {} {} {}", s(d, "state"), s(d, "ref"), s(d, "sha").get(..8).unwrap_or_default())).unwrap_or_default();
                let kind = match s(&e, "kind").as_str() {
                    "" => String::new(),
                    k => {
                        let t = e.get("targets").cloned().unwrap_or_default();
                        let list = |k: &str| t.get(k).and_then(Value::as_array).map(|a| a.iter().filter_map(Value::as_str).collect::<Vec<_>>().join(", ")).unwrap_or_default();
                        let at = [list("clusters"), s(&t, "namespace"), s(&t, "runners"), s(&t, "integration")].into_iter().filter(|x| !x.is_empty()).collect::<Vec<_>>().join(" / ");
                        format!(" {k}{}", if at.is_empty() { String::new() } else { format!(" ({at})") })
                    }
                };
                println!("{:<16} ({}){kind} {}{last}", s(&e, "name"), s(&e, "source"), if rules.is_empty() { "no protection".to_owned() } else { rules.join("; ") });
            }
        }
        EnvCmd::Create(c) => {
            let EnvCreate { name, repo, reviewers, prevent_self_review, wait_timer, branches, tags, url, concurrency, kind, clusters, namespace, runners, integration } = *c;
            let protection = json!({
                "reviewers": reviewers, "prevent-self-review": prevent_self_review, "wait-timer": wait_timer,
                "branches": (!branches.is_empty()).then_some(branches), "tags": (!tags.is_empty()).then_some(tags),
            });
            let targets = json!({ "clusters": clusters, "namespace": namespace, "runners": runners, "integration": integration });
            api.call(Method::Put, &format!("/environments/{}?repo={}", enc(&name), enc(&repo)), Some(json!({ "protection": protection, "url": url, "concurrency": concurrency, "kind": kind, "targets": targets }))).await?;
            println!("{name} saved in {repo}");
        }
        EnvCmd::Starter { repo } => {
            let v = api.call(Method::Post, &format!("/environments/starter?repo={}", enc(&repo)), None).await?;
            let names = |k: &str| v.get(k).and_then(Value::as_array).map(|a| a.iter().filter_map(Value::as_str).collect::<Vec<_>>().join(", ")).unwrap_or_default();
            match (names("made"), names("kept")) {
                (made, kept) if made.is_empty() => println!("{repo} has them all already: {kept}"),
                (made, kept) if kept.is_empty() => println!("made {made} in {repo}"),
                (made, kept) => println!("made {made} in {repo} (it had {kept})"),
            }
        }
        EnvCmd::Delete { name, repo } => {
            api.call(Method::Delete, &format!("/environments/{}?repo={}", enc(&name), enc(&repo)), None).await?;
            println!("removed {name} from {repo}");
        }
        EnvCmd::Deploy { name, repo, git_ref, workflow, inputs } => {
            let mut ask = json!({ "repo": repo, "name": name, "inputs": op_input(&inputs)? });
            if let Some(o) = ask.as_object_mut() {
                for (k, v) in [("ref", git_ref), ("workflow", workflow)] {
                    if let Some(v) = v {
                        o.insert(k.into(), json!(v));
                    }
                }
            }
            let v = run_op(&api, "ci.environments.deploy", ask).await?;
            println!("{} runs at {} to deploy to {name}: `kiln runs --repo {repo}` follows it", s(&v, "workflow"), s(&v, "ref"));
        }
        EnvCmd::Approve { run, environments, reject, comment } => {
            return approve(ApproveArgs { conn: args.conn, run, environments, reject, comment }, cfg).await;
        }
        EnvCmd::Deployments { name, repo } => {
            let v = api.call(Method::Get, &format!("/environments/{}/deployments?repo={}", enc(&name), enc(&repo)), None).await?;
            for d in v.get("deployments").and_then(Value::as_array).cloned().unwrap_or_default() {
                println!("{:>6} {:<12} {:<20} {} {}  {}", d.get("id").and_then(Value::as_i64).unwrap_or_default(), s(&d, "state"), s(&d, "ref"), s(&d, "sha").get(..8).unwrap_or_default(), when(d.get("created_at").and_then(Value::as_i64)), s(&d, "url"));
            }
        }
    }
    Ok(())
}

#[derive(clap::Args)]
pub struct ApproveArgs {
    #[command(flatten)]
    conn: Conn,
    /// The run waiting for a deployment review, or for a maintainer (an outside contributor's).
    run: String,
    /// The environments decided (default: every one the run waits for; repeatable).
    #[arg(long = "env")]
    environments: Vec<String>,
    /// Reject instead: the waiting jobs fail.
    #[arg(long)]
    reject: bool,
    #[arg(long, default_value = "")]
    comment: String,
}

/// `kiln approve`.
pub async fn approve(args: ApproveArgs, cfg: &Config) -> Result<()> {
    let api = args.conn.api(cfg).await?;
    let body = json!({ "environments": args.environments, "state": if args.reject { "rejected" } else { "approved" }, "comment": args.comment });
    let v = api.call(Method::Post, &format!("/runs/{}/approve", enc(&args.run)), Some(body)).await?;
    let verb = if args.reject { "rejected" } else { "approved" };
    // An outside contributor's run (W18-R06), or deployments (W15-R03).
    if v.get("approval").and_then(Value::as_str) == Some("run") {
        println!("{verb} the run{}", if args.reject { ": it is cancelled" } else { ": it starts" });
        return Ok(());
    }
    let envs: Vec<String> = v.get("environments").and_then(Value::as_array).cloned().unwrap_or_default().iter().filter_map(|e| e.as_str().map(str::to_owned)).collect();
    println!("{verb} the deployments to {}", envs.join(", "));
    Ok(())
}

#[derive(clap::Args)]
pub struct DebugArgs {
    #[command(flatten)]
    conn: Conn,
    #[command(subcommand)]
    cmd: DebugCmd,
}

#[derive(clap::Subcommand)]
enum DebugCmd {
    /// A terminal in a running or kept leg (its workspace and environment, or its container),
    /// here: until its shell ends, or Ctrl-] leaves it.
    Terminal { leg: String },
    /// Keep a kept leg longer: MINUTES from now (default 30).
    Keep {
        leg: String,
        #[arg(long, default_value_t = 30)]
        minutes: u32,
    },
    /// Let a kept leg go now: its workspace and containers are removed.
    Release { leg: String },
    /// Run a kept leg's step again in place (after fixing what made it fail, in its terminal), as
    /// a new section of the leg's log.
    RerunStep {
        leg: String,
        /// The step, from 1 as the run page numbers them.
        step: u32,
    },
    /// Run a command in a running or kept leg, as its terminal would (its workspace and
    /// environment, or its container), and exit with its exit code: `kiln debug exec LEG -- ls`.
    Exec {
        leg: String,
        /// Stop it after this many seconds (at most 600).
        #[arg(long, default_value_t = 60)]
        timeout: u64,
        /// The command (shell text; several words are joined with spaces).
        #[arg(last = true, required = true)]
        command: Vec<String>,
    },
}

/// This terminal held raw while a remote one has it (Unix); as it was again when dropped.
struct RawMode(#[cfg(unix)] Option<rustix::termios::Termios>);

impl RawMode {
    #[cfg(unix)]
    fn enter() -> RawMode {
        let stdin = std::io::stdin();
        let Ok(was) = rustix::termios::tcgetattr(&stdin) else { return RawMode(None) };
        let mut raw = was.clone();
        raw.make_raw();
        if rustix::termios::tcsetattr(&stdin, rustix::termios::OptionalActions::Now, &raw).is_err() {
            return RawMode(None);
        }
        RawMode(Some(was))
    }

    /// (Windows' console stays as it is: lines go when Enter is pressed.)
    #[cfg(not(unix))]
    fn enter() -> RawMode {
        RawMode()
    }

    #[cfg(unix)]
    fn restore(&self) {
        if let Some(was) = &self.0 {
            let _ = rustix::termios::tcsetattr(std::io::stdin(), rustix::termios::OptionalActions::Now, was);
        }
    }

    #[cfg(not(unix))]
    fn restore(&self) {}
}

impl Drop for RawMode {
    fn drop(&mut self) {
        self.restore();
    }
}

/// This terminal's size (columns, rows); 120 by 40 when it has none.
fn term_size() -> (u16, u16) {
    #[cfg(unix)]
    if let Ok(w) = rustix::termios::tcgetwinsize(std::io::stdout())
        && w.ws_col > 0
        && w.ws_row > 0
    {
        return (w.ws_col, w.ws_row);
    }
    (120, 40)
}

/// The next keys typed, while there's input to read.
async fn next_key(typed: &async_channel::Receiver<Vec<u8>>, typing: bool) -> Option<Vec<u8>> {
    if typing { typed.recv().await.ok() } else { futures::future::pending().await }
}

/// `kiln debug terminal <leg>` (W17-R01): a terminal in a running or kept leg, in this one, raw
/// (on Unix) and following this window's size, until the shell there ends or Ctrl-] leaves it.
async fn terminal(api: &Api, leg: &str) -> Result<()> {
    use crate::http::ws::Message;
    use futures::{FutureExt as _, StreamExt as _};
    use std::io::Write as _;
    let (cols, rows) = term_size();
    let socket = api.websocket(&format!("/api/ci/legs/{}/terminal?cols={cols}&rows={rows}", enc(leg))).await?;
    let (mut tx, mut rx) = socket.split();
    let raw = RawMode::enter();
    eprint!("[kiln] a terminal in {leg}; Ctrl-] leaves it\r\n");
    // Keys, read on a thread of their own.
    let (keys, typed) = async_channel::bounded::<Vec<u8>>(16);
    std::thread::spawn(move || {
        let mut stdin = std::io::stdin().lock();
        let mut buf = [0u8; 4096];
        while let Ok(n) = stdin.read(&mut buf) {
            if n == 0 || keys.send_blocking(buf.get(..n).unwrap_or_default().to_vec()).is_err() {
                break;
            }
        }
    });
    let mut out = std::io::stdout();
    let (mut size, mut tick) = ((cols, rows), crate::rt::Interval::new(Duration::from_millis(500)));
    let (mut ended, mut typing): (Option<String>, bool) = (None, true);
    loop {
        futures::select_biased! {
            m = rx.next().fuse() => match m {
                Some(Ok(Message::Binary(b))) => {
                    out.write_all(&b)?;
                    out.flush()?;
                }
                Some(Ok(Message::Text(t))) => {
                    let v: Value = serde_json::from_str(t.as_str()).unwrap_or_default();
                    match v.get("type").and_then(Value::as_str).unwrap_or_default() {
                        "exit" => {
                            ended = Some(format!("its shell exited ({})", v.get("code").map_or_else(|| "?".to_owned(), Value::to_string)));
                            break;
                        }
                        "error" | "missing" => {
                            ended = Some(v.get("message").and_then(Value::as_str).unwrap_or("the terminal is gone").to_owned());
                            break;
                        }
                        _ => {}
                    }
                }
                Some(Ok(Message::Close(_))) | None => break,
                Some(Err(e)) => {
                    ended = Some(e.to_string());
                    break;
                }
                Some(Ok(_)) => {}
            },
            k = next_key(&typed, typing).fuse() => match k {
                // Ctrl-] leaves; the session there goes on until it ends.
                Some(bytes) if bytes.contains(&0x1d) => break,
                Some(bytes) => tx.send(Message::binary(bytes)).await?,
                // The input ended (a pipe): what was sent runs on until the shell exits.
                None => typing = false,
            },
            () = tick.tick().fuse() => {
                let now = term_size();
                if now != size {
                    size = now;
                    tx.send(Message::text(json!({ "type": "resize", "cols": now.0, "rows": now.1 }).to_string())).await?;
                }
            },
        }
    }
    drop(raw);
    let _ = tx.close(None).await;
    eprintln!("\r\n[kiln] {}", ended.as_deref().unwrap_or("left the terminal"));
    Ok(())
}

/// `kiln debug`.
pub async fn debug(args: DebugArgs, cfg: &Config) -> Result<()> {
    let api = args.conn.api(cfg).await?;
    match args.cmd {
        DebugCmd::Terminal { leg } => terminal(&api, &leg).await?,
        DebugCmd::Keep { leg, minutes } => {
            let v = api.call(Method::Post, &format!("/legs/{}/keep", enc(&leg)), Some(json!({ "minutes": minutes }))).await?;
            println!("{leg} kept until {}", when(v.get("kept_until").and_then(Value::as_i64)));
        }
        DebugCmd::Release { leg } => {
            api.call(Method::Post, &format!("/legs/{}/release", enc(&leg)), Some(json!({}))).await?;
            println!("released {leg}");
        }
        DebugCmd::Exec { leg, timeout, command } => {
            let body = json!({ "command": command.join(" "), "timeout": timeout });
            let v = api.call_for(Method::Post, &format!("/api/ci/legs/{}/exec", enc(&leg)), Some(body), Duration::from_secs(timeout.min(600) + 30)).await?;
            print!("{}", v.get("output").and_then(Value::as_str).unwrap_or_default());
            if v.get("truncated").and_then(Value::as_bool) == Some(true) {
                eprintln!("[kiln] the output was cut at 1 MiB");
            }
            if v.get("timed_out").and_then(Value::as_bool) == Some(true) {
                eprintln!("[kiln] stopped after {timeout} s");
                return Err(anyhow::Error::new(crate::reach::cli::ExitWith(124)));
            }
            let code = v.get("exit_code").and_then(Value::as_i64).unwrap_or(1);
            if code != 0 {
                return Err(anyhow::Error::new(crate::reach::cli::ExitWith(u8::try_from(code).unwrap_or(1))));
            }
        }
        DebugCmd::RerunStep { leg, step } => {
            let v = api.call(Method::Post, &format!("/legs/{}/rerun-step", enc(&leg)), Some(json!({ "step": step }))).await?;
            println!("step {step} of {leg} runs again as step {} of its log; `kiln logs {leg}` follows it", v.get("idx").and_then(Value::as_u64).map_or(0, |i| i + 1));
        }
    }
    Ok(())
}


#[derive(clap::Args)]
pub struct RepoArgs {
    #[command(flatten)]
    conn: Conn,
    #[command(subcommand)]
    cmd: RepoCmd,
}

#[derive(clap::Subcommand)]
enum RepoCmd {
    /// The repositories kiln follows or hosts.
    List,
    /// A repository kiln hosts (docs/ci.md W14-R01), empty: clone and push it at
    /// `<server>/git/<owner>/<name>.git`.
    Create {
        /// `owner/name`.
        name: String,
        /// Its trunk.
        #[arg(long, default_value = "main")]
        default_branch: String,
    },
    /// An upstream repository's history, copied into one kiln hosts: `kiln repo import
    /// https://github.com/acme/app` (as `acme/app`, or `--name`).
    Import {
        url: String,
        #[arg(long)]
        name: Option<String>,
    },
    /// What a repository's CI runs on (docs/ci.md W23-R07): `kiln repo provider acme/app` says, and
    /// what it may; `kiln repo provider acme/app github-actions` chooses (several may run).
    Provider {
        /// `owner/name`.
        name: String,
        /// `kiln`, `github-actions`, `gitlab-ci`, `bitbucket-pipelines`.
        providers: Vec<String>,
    },
}

/// `owner/name` of a clone URL: `https://host/acme/app.git`, `git@host:acme/app.git`.
fn repo_name_of(url: &str) -> Option<String> {
    let path = url.trim().trim_end_matches('/').trim_end_matches(".git");
    let path = path.split_once("://").map_or_else(|| path.split_once(':').map_or(path, |(_, p)| p), |(_, rest)| rest.split_once('/').map_or("", |(_, p)| p));
    let mut parts = path.rsplit('/');
    let (name, owner) = (parts.next()?, parts.next()?);
    (!name.is_empty() && !owner.is_empty()).then(|| format!("{owner}/{name}"))
}

pub async fn repo(args: RepoArgs, cfg: &Config) -> Result<()> {
    let api = args.conn.api(cfg).await?;
    let created = |v: &Value| {
        let name = v.get("name").and_then(Value::as_str).unwrap_or_default();
        println!("{name}: git clone {}/git/{name}.git", api.url);
    };
    match args.cmd {
        RepoCmd::List => {
            let v = api.call_api(Method::Get, "/api/repos", None).await?;
            for r in v.as_array().cloned().unwrap_or_default() {
                let url = r.get("clone_url").and_then(Value::as_str).unwrap_or_default();
                let mode = if url.starts_with("file://") { "native" } else { url };
                println!("{}\t{mode}", r.get("name").and_then(Value::as_str).unwrap_or_default());
            }
        }
        RepoCmd::Create { name, default_branch } => created(&api.call_api(Method::Post, "/api/repos", Some(json!({ "name": name, "mode": "native", "default_branch": default_branch }))).await?),
        RepoCmd::Import { url, name } => {
            let name = name.or_else(|| repo_name_of(&url)).ok_or_else(|| anyhow::anyhow!("`{url}`: name it with --name owner/name"))?;
            let v = api.call_for(Method::Post, "/api/repos", Some(json!({ "name": name, "mode": "native", "import": url })), Duration::from_secs(1800)).await?;
            created(&v);
        }
        RepoCmd::Provider { name, providers } => {
            let path = format!("/api/ci/repos/{}/providers", enc(&name));
            let shown = providers.is_empty();
            let v = if shown { api.call_api(Method::Get, &path, None).await? } else { api.call_api(Method::Put, &path, Some(json!({ "providers": providers }))).await? };
            let list = |k: &str| v.get(k).and_then(Value::as_array).map(|a| a.iter().filter_map(Value::as_str).collect::<Vec<_>>().join(", ")).unwrap_or_default();
            println!("{name}: {}", list("providers"));
            if shown {
                println!("may have: {}", list("available"));
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod repo_tests {
    use super::repo_name_of;

    #[test]
    fn w14_r01_import_names() {
        assert_eq!(repo_name_of("https://github.com/acme/app.git").as_deref(), Some("acme/app"));
        assert_eq!(repo_name_of("https://git.example.com/group/sub/app/").as_deref(), Some("sub/app"));
        assert_eq!(repo_name_of("git@github.com:acme/app.git").as_deref(), Some("acme/app"));
        assert_eq!(repo_name_of("ssh://git@host:2222/acme/app").as_deref(), Some("acme/app"));
        assert_eq!(repo_name_of("https://github.com/app"), None);
    }
}
