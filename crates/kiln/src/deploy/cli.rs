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
    #[arg(long, visible_alias = "env")]
    to: Option<String>,
    /// An artifact the version deploys: `name=repository:tag`, `name=repository@sha256:…`, or both.
    #[arg(long = "artifact")]
    artifacts: Vec<String>,
    /// A config release: this file becomes the environment's values.yaml.
    #[arg(long)]
    values: Option<PathBuf>,
    /// A promotion: what this environment runs, the same version and artifacts.
    #[arg(long = "from")]
    from: Option<String>,
    /// Return once the release is asked for, without following it.
    #[arg(long)]
    no_wait: bool,
}

#[derive(clap::Subcommand)]
enum DeployCmd {
    /// What runs, what's in flight and what its provider says, for an app in an environment.
    Status {
        app: String,
        #[arg(long, visible_alias = "to")]
        env: String,
    },
    /// Releases, newest first.
    History {
        app: Option<String>,
        #[arg(long, visible_alias = "to")]
        env: Option<String>,
        /// Those of one version.
        #[arg(long)]
        version: Option<String>,
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
        /// The release to go back to.
        #[arg(long)]
        to: Option<i64>,
        #[arg(long)]
        no_wait: bool,
    },
    /// Stop a release that is waiting or being delivered.
    Cancel { release: i64 },
    /// Approve a release waiting for approval.
    Approve {
        release: i64,
        #[arg(long)]
        note: Option<String>,
    },
    /// Reject a release waiting for approval: it is cancelled.
    Reject {
        release: i64,
        #[arg(long)]
        note: Option<String>,
    },
    /// Run the app in the environment again, at its last version.
    Enable {
        app: String,
        #[arg(long, visible_alias = "to")]
        env: String,
    },
    /// Stop running the app in the environment: what it runs is removed, unless `--keep`.
    Disable {
        app: String,
        #[arg(long, visible_alias = "to")]
        env: String,
        #[arg(long)]
        keep: bool,
    },
    /// What an app runs in an environment, as its provider sees it.
    Resources {
        app: String,
        #[arg(long, visible_alias = "to")]
        env: String,
    },
    /// The manifests it renders (or those `--version` would).
    Manifests {
        app: String,
        #[arg(long, visible_alias = "to")]
        env: String,
        #[arg(long)]
        version: Option<String>,
    },
    /// What a sync would change (or what deploying `--version` would).
    Diff {
        app: String,
        #[arg(long, visible_alias = "to")]
        env: String,
        #[arg(long)]
        version: Option<String>,
    },
    /// The last lines of a workload's pods' logs.
    Logs {
        app: String,
        #[arg(long, visible_alias = "to")]
        env: String,
        /// Deployment, StatefulSet, Pod, …
        #[arg(long, default_value = "Deployment")]
        kind: String,
        /// The object's name; the app's by default.
        #[arg(long)]
        name: Option<String>,
        #[arg(long)]
        container: Option<String>,
        #[arg(long, default_value_t = 200)]
        tail: u32,
    },
    /// A fleet change: one version, or values set (`--set path=value`), released to many apps of
    /// an environment at once (those given, or of a template, a project or an owner; all registered
    /// there when none is), each through its gates. With `--show <id>`, how one stands.
    Fleet {
        #[arg(long, visible_alias = "to")]
        env: Option<String>,
        #[arg(long = "app")]
        apps: Vec<String>,
        #[arg(long)]
        template: Option<String>,
        #[arg(long)]
        project: Option<String>,
        #[arg(long)]
        owner: Option<String>,
        #[arg(long, conflicts_with = "set")]
        version: Option<String>,
        /// `path=value` in each app's values: `image.pullPolicy=Always`, `replicaCount=2`.
        #[arg(long = "set")]
        set: Vec<String>,
        /// Asked again with the same key, the same fleet change.
        #[arg(long)]
        key: Option<String>,
        #[arg(long, conflicts_with_all = ["env", "version", "set"])]
        show: Option<String>,
    },
    /// Delivery metrics (DORA's four keys) from the releases: of an app, an environment, or all.
    Metrics {
        #[arg(long)]
        app: Option<String>,
        #[arg(long, visible_alias = "to")]
        env: Option<String>,
        #[arg(long, default_value_t = 30)]
        days: u32,
    },
    /// The versions an app has run in an environment.
    Versions {
        app: String,
        #[arg(long, visible_alias = "to")]
        env: String,
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
    Create(Box<NewApp>),
    /// Register an app in an environment.
    Register {
        app: String,
        #[arg(long, visible_alias = "to")]
        env: String,
    },
    /// Remove an app from an environment: what it runs is removed, unless `--keep`.
    Deregister {
        app: String,
        #[arg(long, visible_alias = "to")]
        env: String,
        #[arg(long)]
        keep: bool,
    },
    /// Turn the provider's automation off for hand edits; the next release turns it back on.
    Pause {
        app: String,
        #[arg(long, visible_alias = "to")]
        env: String,
    },
    Resume {
        app: String,
        #[arg(long, visible_alias = "to")]
        env: String,
    },
    /// Change some of an app: owners, properties, contacts, template, source repository.
    Set {
        app: String,
        /// Its owner teams, replacing them: the owner first, then a co-owner and an ops owner.
        #[arg(long = "owner")]
        owners: Vec<String>,
        /// `name=value`; `name=` removes it.
        #[arg(long = "property")]
        properties: Vec<String>,
        /// Who is on call.
        #[arg(long)]
        oncall: Option<String>,
        /// Chat channels its release messages go to, replacing them.
        #[arg(long = "channel")]
        channels: Vec<String>,
        /// `name=url`, added to its links.
        #[arg(long = "link")]
        links: Vec<String>,
        #[arg(long)]
        template: Option<String>,
        /// Its source repository, one kiln has; `--source ''` for none.
        #[arg(long)]
        source: Option<String>,
    },
    /// The app as Backstage's catalog reads it (catalog-info.yaml).
    Catalog { app: String },
    /// Delete an app: one registered nowhere, or with `--everywhere` deregistered first.
    Delete {
        app: String,
        #[arg(long)]
        everywhere: bool,
        /// With `--everywhere`: leave what it runs in place.
        #[arg(long)]
        keep: bool,
    },
    /// The templates apps are made from.
    Templates,
    /// Add (or replace) a template from a YAML or JSON file.
    TemplateAdd { file: PathBuf },
    TemplateRm { name: String },
    /// An app's chart on kiln's base chart, written into a repository: charts/<app> (kiln-base
    /// vendored in it) and deploy/<env>.yaml, each environment's values to start from. What's
    /// there is kept unless `--force`; `--base` writes only kiln-base, as this kiln ships it.
    Init {
        app: String,
        /// The repository to write into.
        #[arg(long, default_value = ".")]
        dir: PathBuf,
        /// The image it runs (`ghcr.io/acme/web`); each release gives its tag or digest.
        #[arg(long)]
        image: Option<String>,
        /// The port it listens on.
        #[arg(long, default_value_t = 8080)]
        port: u16,
        #[arg(long = "env", default_values_t = ["staging".to_owned(), "production".to_owned()])]
        environments: Vec<String>,
        #[arg(long)]
        force: bool,
        #[arg(long, conflicts_with = "force")]
        base: bool,
    },
}

/// `kiln app create`: an app and how it is deployed.
#[derive(clap::Args)]
struct NewApp {
    app: String,
    /// The template it is made from: its package, namespace and checks, unless given here.
    #[arg(long)]
    template: Option<String>,
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
    /// A platform KilnCD deploys to (aws-lambda, aws-ecs, cloudflare-workers, vercel), with
    /// `--spec`: its definition.
    #[arg(long, conflicts_with_all = ["helm_repo", "git_repo"], requires = "spec")]
    platform: Option<String>,
    /// The platform's definition, a YAML or JSON file, as its key in .kiln/deploy.yaml holds it.
    #[arg(long, requires = "platform")]
    spec: Option<PathBuf>,
    /// Where the platform's own files are (a task definition, wrangler.jsonc), at each
    /// release's version; with `--path`.
    #[arg(long, requires = "platform")]
    files_repo: Option<String>,
    #[arg(long, default_value = "")]
    path: String,
    /// A value: `image.tag=${{ release.artifacts.image.tag }}`.
    #[arg(long = "set")]
    set: Vec<String>,
    #[arg(long)]
    namespace: Option<String>,
    /// Its source repository, one kiln has (`acme/web`); `--source ''` for none. Unset: the one
    /// its template makes from its starter, if it makes one.
    #[arg(long)]
    source: Option<String>,
}

#[derive(clap::Subcommand)]
enum EnvCmd {
    List,
    Show { env: String },
    /// Add (or replace) an environment, deployed from a release repository on kiln's own git
    /// server through KilnCD, or through an Argo CD integration (`--provider argocd/<name>`).
    Add {
        env: String,
        /// `kilncd`, or `argocd/<integration>`.
        #[arg(long, default_value = "kilncd")]
        provider: String,
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
        /// How long the provider may refuse a release before it fails and its commit is
        /// reverted, in seconds.
        #[arg(long, default_value_t = 60)]
        refused: u64,
        /// A release asked for while another is in flight waits, rather than being refused.
        #[arg(long)]
        queue: bool,
        /// Agents may release here (docs/cd-targets.md D6); only a person sets it.
        #[arg(long)]
        agents: bool,
        /// The gates its releases pass (docs/deploy-api.md §9.1), a YAML or JSON file: after, soak,
        /// approvals, windows, checks, freeze.
        #[arg(long)]
        gates: Option<PathBuf>,
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

/// A duration as people say it: `45s`, `3m 20s`, `2h 5m`, `1d 3h`.
fn took(secs: i64) -> String {
    let s = secs.max(0);
    match s {
        0..=59 => format!("{s}s"),
        60..=3_599 => format!("{}m {}s", s / 60, s % 60),
        3_600..=86_399 => format!("{}h {}m", s / 3_600, s % 3_600 / 60),
        _ => format!("{}d {}h", s / 86_400, s % 86_400 / 3_600),
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
            if state == "waiting_approval" {
                println!("        approve it: kiln deploy approve {id}");
            }
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
                let provider = match v.pointer("/provider/kind").and_then(Value::as_str) {
                    Some("argocd") => format!("Argo CD {}", v.pointer("/provider/integration").and_then(Value::as_str).unwrap_or("")),
                    _ => "KilnCD".to_owned(),
                };
                let app = v.pointer("/provider/app").and_then(Value::as_str).unwrap_or("");
                match (p.get("error").and_then(Value::as_str), p.is_null()) {
                    (Some(e), _) => println!("{provider} ({app}): can't be asked: {e}"),
                    (None, true) => println!("{provider} ({app}): nothing there"),
                    (None, false) => println!("{provider} ({app}): {} / {}", s(&p, "sync"), s(&p, "health")),
                }
            });
            return Ok(());
        }
        Some(DeployCmd::History { app, env, version, limit }) => {
            let mut q = vec![format!("limit={limit}")];
            q.extend(app.map(|a| format!("app={}", enc(&a))));
            q.extend(env.map(|e| format!("environment={}", enc(&e))));
            q.extend(version.map(|v| format!("version={}", enc(&v))));
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
        Some(DeployCmd::Fleet { env, apps, template, project, owner, version, set, key, show }) => {
            let v = match show {
                Some(id) => api.call(Method::Get, &format!("/fleets/{}", enc(&id)), None).await?,
                None => {
                    let env = env.context("--env: the environment the fleet change goes to")?;
                    let mut sets = serde_json::Map::new();
                    for kv in &set {
                        let (k, v) = kv.split_once('=').with_context(|| format!("{kv:?}: --set path=value"))?;
                        sets.insert(k.to_owned(), json!(v));
                    }
                    let body = json!({ "environment": env, "apps": apps, "template": template, "project": project, "owner": owner, "version": version, "set": sets, "idempotency_key": key });
                    api.call(Method::Post, "/fleets", Some(body)).await?
                }
            };
            api.print(&v, |v| {
                println!("fleet change {} in {}", s(v, "fleet"), s(v, "environment"));
                for r in v.get("releases").and_then(Value::as_array).into_iter().flatten() {
                    match r.get("release") {
                        Some(rel) => println!("{}  {}", s(rel, "app"), line(rel)),
                        None if r.get("error").is_some() => println!("{}  not released: {}", s(r, "app"), s(r, "error")),
                        None => println!("{}", line(r)),
                    }
                }
                if let Some(states) = v.get("states").and_then(Value::as_object) {
                    let parts: Vec<String> = states.iter().map(|(k, n)| format!("{n} {k}")).collect();
                    println!("{}{}", parts.join(", "), if v.get("done").and_then(Value::as_bool) == Some(true) { " · done" } else { "" });
                }
            });
            return Ok(());
        }
        Some(DeployCmd::Metrics { app, env, days }) => {
            let mut q = vec![format!("days={days}")];
            q.extend(app.map(|a| format!("app={}", enc(&a))));
            q.extend(env.map(|e| format!("environment={}", enc(&e))));
            let v = api.call(Method::Get, &format!("/metrics?{}", q.join("&")), None).await?;
            api.print(&v, |v| {
                let dur = |p: &str| v.pointer(p).and_then(Value::as_i64).map_or_else(|| "-".to_owned(), took);
                let n = |k: &str| v.get(k).and_then(Value::as_i64).unwrap_or(0);
                println!("over {} days: {} changes, {} landed ({:.2} a day)", n("days"), n("changes"), n("landed"), v.get("per_day").and_then(Value::as_f64).unwrap_or(0.0));
                println!("lead time (asked to landed): p50 {}, p90 {}", dur("/lead_time_secs/p50"), dur("/lead_time_secs/p90"));
                match v.get("change_failure_rate").and_then(Value::as_f64) {
                    Some(r) => println!("change failure rate: {:.0}% ({} failed or rolled back)", r * 100.0, n("failed")),
                    None => println!("change failure rate: no changes yet"),
                }
                println!("time to restore: p50 {}, p90 {}", dur("/time_to_restore_secs/p50"), dur("/time_to_restore_secs/p90"));
            });
            return Ok(());
        }
        Some(DeployCmd::Versions { app, env }) => {
            let v = api.call(Method::Get, &format!("/apps/{}/environments/{}/versions", enc(&app), enc(&env)), None).await?;
            api.print(&v, |v| {
                for x in v.get("versions").and_then(Value::as_array).into_iter().flatten() {
                    let runs = if x.get("runs_now").and_then(Value::as_bool) == Some(true) { "  ← runs now" } else { "" };
                    let n = |k: &str| x.get(k).and_then(Value::as_i64).unwrap_or(0);
                    println!("{:<20} {} release(s), last #{}{runs}", s(x, "version"), n("releases"), n("last_release"));
                }
            });
            return Ok(());
        }
        Some(DeployCmd::Resources { app, env }) => {
            let v = api.call(Method::Get, &format!("/apps/{}/environments/{}/resources", enc(&app), enc(&env)), None).await?;
            api.print(&v, |v| {
                for r in v.get("resources").and_then(Value::as_array).into_iter().flatten() {
                    println!("{:<12} {:<10} {:<28} {:<40} {}", s(r, "sync"), s(r, "health"), s(r, "kind"), format!("{}/{}", s(r, "namespace"), s(r, "name")), s(r, "message"));
                }
            });
            return Ok(());
        }
        Some(DeployCmd::Manifests { app, env, version }) => {
            let q = version.map(|v| format!("?version={}", enc(&v))).unwrap_or_default();
            let v = api.call(Method::Get, &format!("/apps/{}/environments/{}/manifests{q}", enc(&app), enc(&env)), None).await?;
            api.print(&v, |v| {
                for m in v.get("manifests").and_then(Value::as_array).into_iter().flatten() {
                    println!("---\n{}", s(m, "yaml").trim_end());
                }
            });
            return Ok(());
        }
        Some(DeployCmd::Diff { app, env, version }) => {
            let q = version.map(|v| format!("?version={}", enc(&v))).unwrap_or_default();
            let v = api.call(Method::Get, &format!("/apps/{}/environments/{}/diff{q}", enc(&app), enc(&env)), None).await?;
            api.print(&v, |v| {
                // `changes` from a server before both providers answered `diffs`.
                let all = v.get("diffs").or_else(|| v.get("changes")).and_then(Value::as_array).cloned().unwrap_or_default();
                let flag = |c: &Value, f: &str| c.get(f).and_then(Value::as_bool).unwrap_or(false);
                let changes: Vec<Value> = all.into_iter().filter(|c| c.get("hunks").and_then(Value::as_array).is_some_and(|h| !h.is_empty()) || flag(c, "missing") || flag(c, "requires_pruning")).collect();
                if changes.is_empty() {
                    println!("nothing would change");
                }
                for c in changes {
                    let k = |f: &str| c.pointer(&format!("/key/{f}")).and_then(Value::as_str).unwrap_or("").to_owned();
                    let note = if flag(&c, "missing") { " (not there yet: a sync makes it)" } else if flag(&c, "requires_pruning") { " (no longer in git: a sync that prunes deletes it)" } else { "" };
                    println!("--- {} {}/{}{note}", k("kind"), k("namespace"), k("name"));
                    for h in c.get("hunks").and_then(Value::as_array).into_iter().flatten() {
                        let n = |f: &str| h.get(f).and_then(Value::as_u64).unwrap_or(0);
                        println!("@@ -{},{} +{},{} @@", n("old_start"), n("old_lines"), n("new_start"), n("new_lines"));
                        for l in h.get("lines").and_then(Value::as_array).into_iter().flatten().filter_map(Value::as_str) {
                            println!("{l}");
                        }
                    }
                }
            });
            return Ok(());
        }
        Some(DeployCmd::Logs { app, env, kind, name, container, tail }) => {
            let name = name.unwrap_or_else(|| app.clone());
            let mut q = format!("?kind={}&name={}&tail={tail}", enc(&kind), enc(&name));
            if let Some(c) = container {
                q.push_str(&format!("&container={}", enc(&c)));
            }
            let v = api.call(Method::Get, &format!("/apps/{}/environments/{}/logs{q}", enc(&app), enc(&env)), None).await?;
            api.print(&v, |v| {
                for l in v.get("lines").and_then(Value::as_array).into_iter().flatten() {
                    println!("{} {}", s(l, "pod"), s(l, "text").trim_end());
                }
            });
            return Ok(());
        }
        Some(DeployCmd::Approve { release, note }) => {
            let v = api.call(Method::Post, &format!("/releases/{release}/approve"), Some(json!({ "note": note }))).await?;
            api.print(&v, |v| println!("{}", line(v)));
            return Ok(());
        }
        Some(DeployCmd::Reject { release, note }) => {
            let v = api.call(Method::Post, &format!("/releases/{release}/reject"), Some(json!({ "note": note }))).await?;
            api.print(&v, |v| println!("{}", line(v)));
            return Ok(());
        }
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
            if let Some(from) = args.from {
                if args.version.is_some() || args.values.is_some() {
                    bail!("--from promotes what another environment runs: no version or --values with it");
                }
                let ask = json!({ "app": app, "environment": env, "promote_from": from });
                let r = api.call(Method::Post, "/releases", Some(ask)).await?;
                if args.no_wait {
                    api.print(&r, |r| println!("{}", line(r)));
                    return Ok(());
                }
                if !follow(&api, r).await? {
                    bail!("the release didn't succeed");
                }
                return Ok(());
            }
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
    // Writes files where it's run: no server is asked.
    if let AppCmd::Init { app, dir, image, port, environments, force, base } = &args.cmd {
        return init(app, dir, image.as_deref(), *port, environments, *force, *base);
    }
    let api = Api::new(cfg, &args.conn).await?;
    match args.cmd {
        // Done above, before any server.
        AppCmd::Init { .. } => {}
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
                if let Some(r) = v.pointer("/app/source").and_then(Value::as_str) {
                    println!("  source       {r}");
                }
                for e in v.get("environments").and_then(Value::as_array).into_iter().flatten() {
                    let env = e.pointer("/registration/environment").and_then(Value::as_str).unwrap_or("");
                    let running = e.get("running").filter(|r| !r.is_null()).map_or_else(|| "nothing released".to_owned(), |r| format!("release {} ({})", r.get("id").and_then(Value::as_i64).unwrap_or(0), s(r, "version")));
                    println!("  {env:<12} {running}");
                }
            });
        }
        AppCmd::Create(new) => {
            let NewApp { app, template, project, owners, helm_repo, chart, chart_version, git_repo, platform, spec, files_repo, path, set, namespace, source } = *new;
            let mut sets = BTreeMap::new();
            for kv in &set {
                let (k, v) = kv.split_once('=').with_context(|| format!("{kv:?}: --set name=value"))?;
                sets.insert(k.to_owned(), v.to_owned());
            }
            let package = match (helm_repo, git_repo, platform) {
                (Some(repo), None, None) => json!({ "kind": "helm", "repo": repo, "chart": chart.context("--chart: the chart's name")?, "chart_version": chart_version, "set": sets }),
                (None, Some(repo), None) => json!({ "kind": "git", "repo": repo, "path": path, "set": sets }),
                (None, None, Some(provider)) => {
                    let file = spec.context("--spec: the platform's definition")?;
                    let text = std::fs::read_to_string(&file).with_context(|| format!("read {}", file.display()))?;
                    let spec: Value = serde_yaml::from_str(&text).with_context(|| format!("{} doesn't read as YAML", file.display()))?;
                    json!({ "kind": "platform", "provider": provider, "spec": spec, "repo": files_repo, "path": path })
                }
                (None, None, None) if template.is_some() => Value::Null,
                _ => bail!("how is it deployed? --helm-repo and --chart, --git-repo (and --path), --platform and --spec, or a --template that says"),
            };
            let body = json!({ "name": app, "project": project, "owners": owners, "package": package, "namespace": namespace, "template": template, "source": source });
            let v = api.call(Method::Post, "/apps", Some(body)).await?;
            api.print(&v, |v| {
                let from = v.get("source").and_then(Value::as_str).map(|r| format!(", its source in {r}")).unwrap_or_default();
                println!("app {}{from} ready: register it in an environment (kiln app register {} --env <env>)", s(v, "name"), s(v, "name"));
            });
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
        AppCmd::Delete { app, everywhere, keep } => {
            api.call(Method::Delete, &format!("/apps/{}?everywhere={everywhere}&keep={keep}", enc(&app)), None).await?;
            if !api.json {
                println!("{app} deleted");
            }
        }
        AppCmd::Set { app, owners, properties, oncall, channels, links, template, source } => {
            let mut patch = serde_json::Map::new();
            if !owners.is_empty() {
                patch.insert("owners".into(), json!(owners));
            }
            if !properties.is_empty() {
                let mut props = serde_json::Map::new();
                for kv in &properties {
                    let (k, v) = kv.split_once('=').with_context(|| format!("{kv:?}: --property name=value"))?;
                    props.insert(k.to_owned(), if v.is_empty() { Value::Null } else { json!(v) });
                }
                patch.insert("properties".into(), Value::Object(props));
            }
            if oncall.is_some() || !channels.is_empty() || !links.is_empty() {
                let now = api.call(Method::Get, &format!("/apps/{}", enc(&app)), None).await?;
                let mut contacts = now.pointer("/app/contacts").cloned().unwrap_or_else(|| json!({}));
                if let Some(m) = contacts.as_object_mut() {
                    if let Some(o) = oncall {
                        m.insert("oncall".into(), json!(o));
                    }
                    if !channels.is_empty() {
                        m.insert("channels".into(), json!(channels));
                    }
                    for kv in &links {
                        let (k, v) = kv.split_once('=').with_context(|| format!("{kv:?}: --link name=url"))?;
                        let entry = m.entry("links").or_insert_with(|| json!({}));
                        if let Some(l) = entry.as_object_mut() {
                            l.insert(k.to_owned(), json!(v));
                        }
                    }
                }
                patch.insert("contacts".into(), contacts);
            }
            if let Some(t) = template {
                patch.insert("template".into(), json!(t));
            }
            if let Some(r) = source {
                patch.insert("source".into(), json!(r));
            }
            let v = api.call(Method::Patch, &format!("/apps/{}", enc(&app)), Some(Value::Object(patch))).await?;
            api.print(&v, |v| println!("{} updated", s(v, "name")));
        }
        AppCmd::Catalog { app } => {
            let v = api.call(Method::Get, &format!("/apps/{}/catalog-info.yaml", enc(&app)), None).await?;
            print!("{}", v.as_str().unwrap_or_default());
        }
        AppCmd::Templates => {
            let v = api.call(Method::Get, "/templates", None).await?;
            api.print(&v, |v| {
                for t in v.get("templates").and_then(Value::as_array).into_iter().flatten() {
                    println!("{:<20} {}", s(t, "name"), s(t, "description"));
                }
            });
        }
        AppCmd::TemplateAdd { file } => {
            let text = std::fs::read_to_string(&file).with_context(|| format!("read {}", file.display()))?;
            let t: Value = serde_yaml::from_str(&text).with_context(|| format!("{} doesn't read as YAML", file.display()))?;
            let name = s(&t, "name").to_owned();
            let v = api.call(Method::Put, &format!("/templates/{}", enc(&name)), Some(t)).await?;
            api.print(&v, |v| println!("template {} ready", s(v, "name")));
        }
        AppCmd::TemplateRm { name } => {
            api.call(Method::Delete, &format!("/templates/{}", enc(&name)), None).await?;
            if !api.json {
                println!("template {name} removed");
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
                        "{:<16} {:<18} {}@{}  {}",
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
        EnvCmd::Add { env, provider, repo, branch, root, clusters, no_prune, self_heal, timeout, settle, refused, queue, agents, gates } => {
            let gates: Value = match gates {
                Some(path) => {
                    let text = std::fs::read_to_string(&path).with_context(|| format!("read {}", path.display()))?;
                    serde_yaml::from_str(&text).with_context(|| format!("{} doesn't read as YAML", path.display()))?
                }
                None => Value::Null,
            };
            let body = json!({
                "name": env,
                "provider": provider,
                "release_repo": { "host": "kiln", "repo": repo, "branch": branch, "root": root },
                "targets": { "kubernetes": clusters },
                "sync": { "prune": !no_prune, "self_heal": self_heal },
                "watch": { "timeout_secs": timeout, "settle_secs": settle, "refused_secs": refused },
                "concurrency": if queue { "queue" } else { "reject" },
                "agents": { "deploy": agents },
                "gates": gates,
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

/// `kiln app init`: an app's chart on kiln's base chart, written into the repository at `dir`.
fn init(app: &str, dir: &std::path::Path, image: Option<&str>, port: u16, environments: &[String], force: bool, base: bool) -> Result<()> {
    let label = !app.is_empty() && app.len() <= 63 && app.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-') && !app.starts_with('-') && !app.ends_with('-');
    if !label {
        bail!("{app:?}: an app's name is lowercase letters, digits and '-', as Kubernetes names things");
    }
    let image = image.map_or_else(|| format!("registry.example.com/{app}"), str::to_owned);
    let files = super::init::files(&super::init::Init { app, image: &image, port, environments });
    let base_dir = std::path::Path::new("charts").join(app).join("charts/kiln-base");
    let chosen: Vec<(PathBuf, String)> = if base { files.into_iter().filter(|(p, _)| p.starts_with(&base_dir)).collect() } else { files };
    let (wrote, kept) = super::init::write(dir, &chosen, force || base).with_context(|| format!("write into {}", dir.display()))?;
    for p in &wrote {
        println!("wrote {}", p.display());
    }
    for p in &kept {
        println!("kept {} (it was there: --force writes over it)", p.display());
    }
    if !base {
        println!(
            "\nNext: commit it, then\n  kiln app create {app} --git-repo <this repository's URL> --path charts/{app} --set 'image.tag=${{{{ release.version }}}}'\n  kiln app register {app} --env {env}\n  kiln deploy {app} <a tag or commit your CI built> --to {env}",
            env = environments.first().map_or("staging", String::as_str)
        );
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
