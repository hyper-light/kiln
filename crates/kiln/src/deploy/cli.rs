//! `kiln deploy` and `kiln app` (docs/deploy-api.md §12.2), over the server's `/api/deploy`.
//! Environments are `kiln deploy env`: `kiln env` is CI's (a repository's environments, ci.md
//! W15), until the two become one.
//!
//! ```sh
//! kiln deploy env add staging --cluster dev
//! kiln app create web --project shop --owner shop --helm-repo oci://ghcr.io/acme/charts --chart web \
//!     --set 'image.tag=${{ release.artifacts.image.tag }}'
//! kiln app register web --env staging
//! kiln deploy web 1.4.2 --to staging --artifact image=ghcr.io/acme/web:1.4.2
//! kiln deploy web --to staging --values values.yaml
//! kiln deploy rollback web --env staging
//! ```

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::time::Duration;

use anyhow::{Context as _, Result, bail};
use serde_json::{Value, json};

use crate::config::Config;
use crate::http::client::{Client, Method};

/// Where the server is, and how to show answers.
#[derive(clap::Args, Clone)]
pub struct Conn {
    /// The server's URL, e.g. http://localhost:7070.
    #[arg(long, env = "KILN_URL", global = true)]
    url: Option<String>,
    /// The server's admin token.
    #[arg(long, env = "KILN_ADMIN_TOKEN", hide_env_values = true, global = true)]
    token: Option<String>,
    /// Print JSON.
    #[arg(long, global = true)]
    json: bool,
}

/// `kiln deploy`: release an app, and follow it.
#[derive(clap::Args)]
#[command(args_conflicts_with_subcommands = true)]
pub struct DeployArgs {
    #[command(flatten)]
    conn: Conn,
    #[command(subcommand)]
    cmd: Option<DeployCmd>,
    /// The app.
    app: Option<String>,
    /// The version to deploy. Without one, `--values` makes a config release.
    version: Option<String>,
    /// The environment.
    #[arg(long)]
    to: Option<String>,
    /// An artifact the version deploys: `name=repository:tag`, `name=repository@sha256:…`, or both.
    #[arg(long = "artifact")]
    artifacts: Vec<String>,
    /// A config release: this file becomes the environment's values.yaml.
    #[arg(long)]
    values: Option<PathBuf>,
    /// Return once the release is asked for, without following it.
    #[arg(long)]
    no_wait: bool,
}

#[derive(clap::Subcommand)]
enum DeployCmd {
    /// What runs, what's in flight and what KilnCD says, for an app in an environment.
    Status {
        app: String,
        #[arg(long)]
        env: String,
    },
    /// Releases, newest first.
    History {
        app: Option<String>,
        #[arg(long)]
        env: Option<String>,
        #[arg(long, default_value_t = 20)]
        limit: usize,
    },
    /// One release.
    Show { release: i64 },
    /// Go back to an earlier release (by default the newest that succeeded before what runs).
    Rollback {
        app: String,
        #[arg(long)]
        env: String,
        #[arg(long)]
        to: Option<i64>,
        #[arg(long)]
        no_wait: bool,
    },
    /// Stop a release that is waiting or being delivered.
    Cancel { release: i64 },
    /// Run the app in the environment again, at its last version.
    Enable {
        app: String,
        #[arg(long)]
        env: String,
    },
    /// Stop running the app in the environment: what it runs is removed, unless `--keep`.
    Disable {
        app: String,
        #[arg(long)]
        env: String,
        #[arg(long)]
        keep: bool,
    },
    /// The environments apps are released to.
    Env {
        #[command(subcommand)]
        cmd: EnvCmd,
    },
}

/// `kiln app`: apps, and where they're registered.
#[derive(clap::Args)]
pub struct AppArgs {
    #[command(flatten)]
    conn: Conn,
    #[command(subcommand)]
    cmd: AppCmd,
}

#[derive(clap::Subcommand)]
enum AppCmd {
    /// Every app.
    List,
    /// An app, and what runs where.
    Show { app: String },
    /// Create (or replace) an app.
    Create {
        app: String,
        #[arg(long, default_value = "default")]
        project: String,
        /// Owner teams: the owner first, then a co-owner and an ops owner.
        #[arg(long = "owner")]
        owners: Vec<String>,
        /// A Helm chart's repository (`oci://…` or a Helm repository's URL).
        #[arg(long, conflicts_with = "git_repo")]
        helm_repo: Option<String>,
        #[arg(long, requires = "helm_repo")]
        chart: Option<String>,
        /// The chart's version, when it isn't the release's (`${{ release.version }}`).
        #[arg(long, requires = "helm_repo")]
        chart_version: Option<String>,
        /// A git repository whose path deploys at the release's version (a ref).
        #[arg(long)]
        git_repo: Option<String>,
        #[arg(long, default_value = "")]
        path: String,
        /// A value: `image.tag=${{ release.artifacts.image.tag }}`.
        #[arg(long = "set")]
        set: Vec<String>,
        #[arg(long)]
        namespace: Option<String>,
    },
    /// Register an app in an environment.
    Register {
        app: String,
        #[arg(long)]
        env: String,
    },
    /// Remove an app from an environment: what it runs is removed, unless `--keep`.
    Deregister {
        app: String,
        #[arg(long)]
        env: String,
        #[arg(long)]
        keep: bool,
    },
    /// Turn the provider's automation off for hand edits; the next release turns it back on.
    Pause {
        app: String,
        #[arg(long)]
        env: String,
    },
    Resume {
        app: String,
        #[arg(long)]
        env: String,
    },
    /// Delete an app registered nowhere.
    Delete { app: String },
}

#[derive(clap::Subcommand)]
enum EnvCmd {
    List,
    Show { env: String },
    /// Add (or replace) an environment, deployed through KilnCD from a release repository on
    /// kiln's own git server.
    Add {
        env: String,
        /// The release repository on kiln's git server.
        #[arg(long, default_value = "kiln/releases")]
        repo: String,
        #[arg(long, default_value = "main")]
        branch: String,
        #[arg(long, default_value = "")]
        root: String,
        /// A cluster it deploys to (KilnCD's name); none: the cluster kiln runs in.
        #[arg(long = "cluster")]
        clusters: Vec<String>,
        /// Don't delete what git no longer holds.
        #[arg(long)]
        no_prune: bool,
        /// Undo hand edits.
        #[arg(long)]
        self_heal: bool,
        /// How long a release has to land, in seconds.
        #[arg(long, default_value_t = 900)]
        timeout: u64,
        /// How long a landed release must stay synced and healthy, in seconds.
        #[arg(long, default_value_t = 30)]
        settle: u64,
        /// A release asked for while another is in flight waits, rather than being refused.
        #[arg(long)]
        queue: bool,
    },
    Rm { env: String },
}

struct Api {
    client: Client,
    url: String,
    token: String,
    json: bool,
}

impl Api {
    async fn new(cfg: &Config, c: &Conn) -> Result<Self> {
        let t = crate::access::resolve(cfg, c.url.as_deref(), c.token.as_deref()).await?;
        Ok(Self { client: Client::public()?, url: t.url, token: t.token, json: c.json })
    }

    async fn call(&self, method: Method, path: &str, body: Option<Value>) -> Result<Value> {
        let mut req = self.client.request(method, &format!("{}/api/deploy{path}", self.url)).bearer(&self.token).timeout(Duration::from_secs(60));
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

    fn print(&self, v: &Value, human: impl FnOnce(&Value)) {
        if self.json {
            println!("{}", serde_json::to_string_pretty(v).unwrap_or_default());
        } else {
            human(v);
        }
    }
}

fn s<'a>(v: &'a Value, k: &str) -> &'a str {
    v.get(k).and_then(Value::as_str).unwrap_or("")
}

fn enc(s: &str) -> String {
    kiln_cd::kube::enc(s)
}

/// `name=repository:tag`, `name=repository@digest` or `name=repository:tag@digest`.
fn artifact(spec: &str) -> Result<(String, Value)> {
    let (name, image) = spec.split_once('=').with_context(|| format!("{spec:?}: name=repository:tag[@digest]"))?;
    let (image, digest) = image.split_once('@').map_or((image, ""), |(i, d)| (i, d));
    // A tag follows the last ':' after the last '/', so a registry's port isn't one.
    let slash = image.rfind('/').map_or(0, |i| i + 1);
    let (repository, tag) = match image.get(slash..).and_then(|last| last.rfind(':')) {
        Some(i) => (image.get(..slash + i).unwrap_or(image), image.get(slash + i + 1..).unwrap_or("")),
        None => (image, ""),
    };
    Ok((name.to_owned(), json!({ "repository": repository, "tag": tag, "digest": digest })))
}

fn line(r: &Value) -> String {
    let what = match s(r, "kind") {
        "deploy" => format!("deploy {}", s(r, "version")),
        "rollback" => format!("rollback to {}{}", s(r, "version"), r.get("rollback_of").and_then(Value::as_i64).map(|t| format!(" (release {t})")).unwrap_or_default()),
        k => k.to_owned(),
    };
    let commit = s(r, "commit").get(..12).unwrap_or("");
    let msg = s(r, "message");
    format!(
        "{:>6}  {:<10} {:<12} {}  {}{}{}",
        r.get("id").and_then(Value::as_i64).unwrap_or(0),
        s(r, "environment"),
        s(r, "state"),
        s(r, "app"),
        what,
        if commit.is_empty() { String::new() } else { format!("  {commit}") },
        if msg.is_empty() { String::new() } else { format!("  — {msg}") }
    )
}

/// Follows a release to its end, printing each state it takes; whether it succeeded.
async fn follow(api: &Api, mut r: Value) -> Result<bool> {
    let id = r.get("id").and_then(Value::as_i64).context("the server answered without a release")?;
    let mut last = String::new();
    loop {
        let state = s(&r, "state").to_owned();
        let shown = format!("{state} {}", s(&r, "message"));
        if shown != last && !api.json {
            println!("{}", line(&r));
            last = shown;
        }
        match state.as_str() {
            "succeeded" => return Ok(true),
            "failed" | "cancelled" | "rolled_back" | "superseded" => return Ok(false),
            _ => {}
        }
        crate::rt::sleep(Duration::from_secs(2)).await;
        r = api.call(Method::Get, &format!("/releases/{id}"), None).await?;
    }
}

/// Runs `kiln deploy`.
pub async fn deploy(args: DeployArgs, cfg: &Config) -> Result<()> {
    let api = Api::new(cfg, &args.conn).await?;
    let ask = match args.cmd {
        Some(DeployCmd::Status { app, env }) => {
            let v = api.call(Method::Get, &format!("/apps/{}/environments/{}", enc(&app), enc(&env)), None).await?;
            api.print(&v, |v| {
                let running = v.get("running").filter(|r| !r.is_null());
                println!("{app} in {env}: {}", running.map_or_else(|| "nothing released yet".to_owned(), |r| format!("release {} ({})", r.get("id").and_then(Value::as_i64).unwrap_or(0), s(r, "version"))));
                if let Some(f) = v.get("in_flight").filter(|r| !r.is_null()) {
                    println!("in flight: {}", line(f));
                }
                let p = v.pointer("/provider/status").cloned().unwrap_or(Value::Null);
                if !p.is_null() {
                    println!("KilnCD ({}): {} / {}", v.pointer("/provider/app").and_then(Value::as_str).unwrap_or(""), s(&p, "sync"), s(&p, "health"));
                }
            });
            return Ok(());
        }
        Some(DeployCmd::History { app, env, limit }) => {
            let mut q = vec![format!("limit={limit}")];
            q.extend(app.map(|a| format!("app={}", enc(&a))));
            q.extend(env.map(|e| format!("environment={}", enc(&e))));
            let v = api.call(Method::Get, &format!("/releases?{}", q.join("&")), None).await?;
            api.print(&v, |v| {
                for r in v.get("releases").and_then(Value::as_array).into_iter().flatten() {
                    println!("{}", line(r));
                }
            });
            return Ok(());
        }
        Some(DeployCmd::Show { release }) => {
            let v = api.call(Method::Get, &format!("/releases/{release}"), None).await?;
            api.print(&v, |v| println!("{}", line(v)));
            return Ok(());
        }
        Some(DeployCmd::Env { cmd }) => return env(&api, cmd).await,
        Some(DeployCmd::Cancel { release }) => {
            let v = api.call(Method::Post, &format!("/releases/{release}/cancel"), None).await?;
            api.print(&v, |v| println!("{}", line(v)));
            return Ok(());
        }
        Some(DeployCmd::Rollback { app, env, to, no_wait }) => (json!({ "app": app, "environment": env, "rollback": { "to": to } }), no_wait),
        Some(DeployCmd::Enable { app, env }) => (json!({ "app": app, "environment": env, "enabled": true }), false),
        Some(DeployCmd::Disable { app, env, keep }) => (json!({ "app": app, "environment": env, "enabled": false, "keep": keep }), false),
        None => {
            let app = args.app.context("which app? kiln deploy <app> <version> --to <env>")?;
            let env = args.to.context("which environment? --to <env>")?;
            let ask = match (args.version, args.values) {
                (Some(v), None) => {
                    let mut arts = serde_json::Map::new();
                    for a in &args.artifacts {
                        let (name, spec) = artifact(a)?;
                        arts.insert(name, spec);
                    }
                    json!({ "app": app, "environment": env, "version": v, "artifacts": arts })
                }
                (None, Some(path)) => {
                    let values = std::fs::read_to_string(&path).with_context(|| format!("read {}", path.display()))?;
                    json!({ "app": app, "environment": env, "values": values })
                }
                (Some(_), Some(_)) => bail!("a version or --values, not both: deploy the version first, then change its values"),
                (None, None) => bail!("what to release? a version (kiln deploy <app> <version> --to <env>), or --values <file>"),
            };
            (ask, args.no_wait)
        }
    };
    let (ask, no_wait) = ask;
    let r = api.call(Method::Post, "/releases", Some(ask)).await?;
    if no_wait {
        api.print(&r, |r| println!("{}", line(r)));
        return Ok(());
    }
    if !follow(&api, r).await? {
        bail!("the release didn't succeed");
    }
    Ok(())
}

/// Runs `kiln app`.
pub async fn app(args: AppArgs, cfg: &Config) -> Result<()> {
    let api = Api::new(cfg, &args.conn).await?;
    match args.cmd {
        AppCmd::List => {
            let v = api.call(Method::Get, "/apps", None).await?;
            api.print(&v, |v| {
                for a in v.get("apps").and_then(Value::as_array).into_iter().flatten() {
                    let owners: Vec<&str> = a.get("owners").and_then(Value::as_array).into_iter().flatten().filter_map(Value::as_str).collect();
                    println!("{:<24} {:<12} {}", s(a, "name"), s(a, "project"), owners.join(", "));
                }
            });
        }
        AppCmd::Show { app } => {
            let v = api.call(Method::Get, &format!("/apps/{}", enc(&app)), None).await?;
            api.print(&v, |v| {
                println!("{app} ({})", v.pointer("/app/project").and_then(Value::as_str).unwrap_or(""));
                for e in v.get("environments").and_then(Value::as_array).into_iter().flatten() {
                    let env = e.pointer("/registration/environment").and_then(Value::as_str).unwrap_or("");
                    let running = e.get("running").filter(|r| !r.is_null()).map_or_else(|| "nothing released".to_owned(), |r| format!("release {} ({})", r.get("id").and_then(Value::as_i64).unwrap_or(0), s(r, "version")));
                    println!("  {env:<12} {running}");
                }
            });
        }
        AppCmd::Create { app, project, owners, helm_repo, chart, chart_version, git_repo, path, set, namespace } => {
            let mut sets = BTreeMap::new();
            for kv in &set {
                let (k, v) = kv.split_once('=').with_context(|| format!("{kv:?}: --set name=value"))?;
                sets.insert(k.to_owned(), v.to_owned());
            }
            let package = match (helm_repo, git_repo) {
                (Some(repo), None) => json!({ "kind": "helm", "repo": repo, "chart": chart.context("--chart: the chart's name")?, "chart_version": chart_version, "set": sets }),
                (None, Some(repo)) => json!({ "kind": "git", "repo": repo, "path": path, "set": sets }),
                _ => bail!("how is it deployed? --helm-repo and --chart, or --git-repo (and --path)"),
            };
            let body = json!({ "name": app, "project": project, "owners": owners, "package": package, "namespace": namespace });
            let v = api.call(Method::Post, "/apps", Some(body)).await?;
            api.print(&v, |v| println!("app {} ready: register it in an environment (kiln app register {} --env <env>)", s(v, "name"), s(v, "name")));
        }
        AppCmd::Register { app, env } => {
            let v = api.call(Method::Post, &format!("/apps/{}/environments/{}", enc(&app), enc(&env)), None).await?;
            api.print(&v, |_| println!("{app} registered in {env}: deploy it (kiln deploy {app} <version> --to {env})"));
        }
        AppCmd::Deregister { app, env, keep } => {
            api.call(Method::Delete, &format!("/apps/{}/environments/{}{}", enc(&app), enc(&env), if keep { "?keep=true" } else { "" }), None).await?;
            if !api.json {
                println!("{app} deregistered from {env}{}", if keep { "; what it ran is kept" } else { "" });
            }
        }
        AppCmd::Pause { app, env } => {
            let v = api.call(Method::Post, &format!("/apps/{}/environments/{}/pause", enc(&app), enc(&env)), None).await?;
            api.print(&v, |_| println!("{app} in {env}: automation paused until the next release (kiln app resume {app} --env {env})"));
        }
        AppCmd::Resume { app, env } => {
            let v = api.call(Method::Post, &format!("/apps/{}/environments/{}/resume", enc(&app), enc(&env)), None).await?;
            api.print(&v, |_| println!("{app} in {env}: automation resumed"));
        }
        AppCmd::Delete { app } => {
            api.call(Method::Delete, &format!("/apps/{}", enc(&app)), None).await?;
            if !api.json {
                println!("{app} deleted");
            }
        }
    }
    Ok(())
}

/// Runs `kiln deploy env`.
async fn env(api: &Api, cmd: EnvCmd) -> Result<()> {
    match cmd {
        EnvCmd::List => {
            let v = api.call(Method::Get, "/environments", None).await?;
            api.print(&v, |v| {
                for e in v.get("environments").and_then(Value::as_array).into_iter().flatten() {
                    let clusters: Vec<&str> = e.pointer("/targets/kubernetes").and_then(Value::as_array).into_iter().flatten().filter_map(Value::as_str).collect();
                    println!(
                        "{:<16} {:<8} {}@{}  {}",
                        s(e, "name"),
                        s(e, "provider"),
                        e.pointer("/release_repo/repo").and_then(Value::as_str).unwrap_or(""),
                        e.pointer("/release_repo/branch").and_then(Value::as_str).unwrap_or(""),
                        if clusters.is_empty() { "in-cluster".to_owned() } else { clusters.join(", ") }
                    );
                }
            });
        }
        EnvCmd::Show { env } => {
            let v = api.call(Method::Get, &format!("/environments/{}", enc(&env)), None).await?;
            println!("{}", serde_json::to_string_pretty(&v).unwrap_or_default());
        }
        EnvCmd::Add { env, repo, branch, root, clusters, no_prune, self_heal, timeout, settle, queue } => {
            let body = json!({
                "name": env,
                "provider": "kilncd",
                "release_repo": { "host": "kiln", "repo": repo, "branch": branch, "root": root },
                "targets": { "kubernetes": clusters },
                "sync": { "prune": !no_prune, "self_heal": self_heal },
                "watch": { "timeout_secs": timeout, "settle_secs": settle },
                "concurrency": if queue { "queue" } else { "reject" },
            });
            let v = api.call(Method::Post, "/environments", Some(body)).await?;
            api.print(&v, |v| println!("environment {} ready", s(v, "name")));
        }
        EnvCmd::Rm { env } => {
            api.call(Method::Delete, &format!("/environments/{}", enc(&env)), None).await?;
            if !api.json {
                println!("environment {env} removed");
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn artifacts_read_with_ports_tags_and_digests() -> Result<()> {
        let (name, a) = artifact("image=ghcr.io/acme/web:1.4.2")?;
        assert_eq!((name.as_str(), s(&a, "repository"), s(&a, "tag")), ("image", "ghcr.io/acme/web", "1.4.2"));
        let (_, a) = artifact("image=registry:5000/web@sha256:abc")?;
        assert_eq!((s(&a, "repository"), s(&a, "tag"), s(&a, "digest")), ("registry:5000/web", "", "sha256:abc"));
        let (_, a) = artifact("image=registry:5000/web:2.0@sha256:abc")?;
        assert_eq!((s(&a, "repository"), s(&a, "tag")), ("registry:5000/web", "2.0"));
        assert!(artifact("no-equals").is_err());
        Ok(())
    }
}
