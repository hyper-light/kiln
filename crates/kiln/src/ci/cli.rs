//! `kiln secrets` and `kiln vars` (docs/ci.md W09-R11), over the server's API. A scope is
//! written `server` (the default), `org:<owner>`, `repo:<owner/name>`, `workflow:<repo>:<path>`
//! or `env:<repo>:<environment>`.

use std::io::Read as _;
use std::time::Duration;

use anyhow::{Result, bail};
use serde_json::{Value, json};

use crate::config::Config;
use crate::http::client::{Client, Method};

#[derive(clap::Args)]
pub struct SecretsArgs {
    /// The server's URL, e.g. http://localhost:7070.
    #[arg(long, env = "KILN_URL", global = true)]
    url: Option<String>,
    /// The server's admin token.
    #[arg(long, env = "KILN_ADMIN_TOKEN", hide_env_values = true, global = true)]
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
    /// Set a secret from a file, or from stdin (one trailing newline is dropped).
    Set {
        name: String,
        #[arg(long, default_value = "server")]
        scope: String,
        #[arg(long)]
        from_file: Option<std::path::PathBuf>,
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
    #[arg(long, env = "KILN_ADMIN_TOKEN", hide_env_values = true, global = true)]
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

    /// A CI route (`/api/ci<path>`).
    async fn call(&self, method: Method, path: &str, body: Option<Value>) -> Result<Value> {
        self.call_api(method, &format!("/api/ci{path}"), body).await
    }

    /// Any route of the API (`path` from `/api`).
    pub(crate) async fn call_api(&self, method: Method, path: &str, body: Option<Value>) -> Result<Value> {
        let mut req = self.client.request(method, &format!("{}{path}", self.url)).bearer(&self.token).timeout(Duration::from_secs(60));
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
    #[arg(long, env = "KILN_ADMIN_TOKEN", hide_env_values = true, global = true)]
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
    /// Write the discovery document and keys to a directory for a static host (W10-R12): serve it
    /// at the issuer URL (`[ci.oidc] issuer`). Run it again within 7 days of each new key (`kiln
    /// oidc status`), or set `[ci.oidc] publish_dir` and the server keeps it written.
    Publish {
        #[arg(long)]
        to: std::path::PathBuf,
    },
    /// Make a new signing key: it signs in 7 days, or at once with `--now`.
    Rotate {
        #[arg(long)]
        now: bool,
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
        OidcCmd::Publish { to } => {
            let v = api.call(Method::Get, "/oidc/jwks-export", None).await?;
            let (Some(discovery), Some(jwks)) = (v.get("discovery"), v.get("jwks")) else { bail!("the server sent no documents") };
            super::oidc::write_published(&to, discovery, jwks)?;
            println!("wrote the discovery document and keys to {}; serve it at {}", to.display(), v.get("issuer").and_then(Value::as_str).unwrap_or_default());
        }
        OidcCmd::Rotate { now } => {
            let v = api.call(Method::Post, "/oidc/rotate", Some(json!({ "now": now }))).await?;
            println!("a new signing key, signing from {}", when(v.get("sign_from").and_then(Value::as_i64)));
        }
    }
    Ok(())
}

#[derive(clap::Args)]
pub struct ActionsArgs {
    #[arg(long, env = "KILN_URL", global = true)]
    url: Option<String>,
    #[arg(long, env = "KILN_ADMIN_TOKEN", hide_env_values = true, global = true)]
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
#[derive(clap::Args)]
pub struct Conn {
    #[arg(long, env = "KILN_URL", global = true)]
    url: Option<String>,
    #[arg(long, env = "KILN_ADMIN_TOKEN", hide_env_values = true, global = true)]
    token: Option<String>,
}

impl Conn {
    async fn api(&self, cfg: &Config) -> Result<Api> {
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
}

pub async fn rerun(args: RerunArgs, cfg: &Config) -> Result<()> {
    let api = args.conn.api(cfg).await?;
    let mode = if args.failed { "failed" } else if args.job.is_some() { "job" } else { "all" };
    let v = api.call(Method::Post, &format!("/runs/{}/rerun", enc(&args.run)), Some(json!({ "mode": mode, "job": args.job, "debug": args.debug, "fresh_actions": args.fresh_actions }))).await?;
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
        /// Constraints of its own (`requires:`), e.g. `"memory >= 16Gi"`.
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
fn dotenv(text: &str) -> Vec<(String, String)> {
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
        SecretsCmd::Set { name, scope, from_file } => {
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

#[derive(clap::Subcommand)]
enum EnvCmd {
    /// A repository's environments, their protection and latest deployment.
    List {
        #[arg(long)]
        repo: String,
    },
    /// Make an environment in kiln (or change one made here), with its protection rules.
    Create {
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
                let last = e.get("latest_deployment").filter(|d| !d.is_null()).map(|d| format!("; last: {} {} {}", s(d, "state"), s(d, "ref"), s(d, "sha").get(..8).unwrap_or_default())).unwrap_or_default();
                println!("{:<16} ({}) {}{last}", s(&e, "name"), s(&e, "source"), if rules.is_empty() { "no protection".to_owned() } else { rules.join("; ") });
            }
        }
        EnvCmd::Create { name, repo, reviewers, prevent_self_review, wait_timer, branches, tags, url } => {
            let protection = json!({
                "reviewers": reviewers, "prevent-self-review": prevent_self_review, "wait-timer": wait_timer,
                "branches": (!branches.is_empty()).then_some(branches), "tags": (!tags.is_empty()).then_some(tags),
            });
            api.call(Method::Put, &format!("/environments/{}?repo={}", enc(&name), enc(&repo)), Some(json!({ "protection": protection, "url": url }))).await?;
            println!("{name} saved in {repo}");
        }
        EnvCmd::Delete { name, repo } => {
            api.call(Method::Delete, &format!("/environments/{}?repo={}", enc(&name), enc(&repo)), None).await?;
            println!("removed {name} from {repo}");
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
    /// The run waiting for a deployment review.
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
    let envs: Vec<String> = v.get("environments").and_then(Value::as_array).cloned().unwrap_or_default().iter().filter_map(|e| e.as_str().map(str::to_owned)).collect();
    println!("{} the deployments to {}", if args.reject { "rejected" } else { "approved" }, envs.join(", "));
    Ok(())
}

