//! `kiln cd`: deployments from a terminal, over the server's API (`/api/cd`), and the fleet
//! benchmark, which runs here without a server.

use std::time::Duration;

use anyhow::{Context as _, Result, bail};
use serde_json::{Value, json};

use crate::config::Config;
use crate::http::client::{Client, Method};

/// `kiln cd`.
#[derive(clap::Args)]
pub struct CdArgs {
    /// The server's URL, e.g. http://localhost:7070.
    #[arg(long, env = "KILN_URL", global = true)]
    url: Option<String>,
    /// The server's admin token.
    #[arg(long, env = "KILN_ADMIN_TOKEN", hide_env_values = true, global = true)]
    token: Option<String>,
    /// Print JSON.
    #[arg(long, global = true)]
    json: bool,
    #[command(subcommand)]
    cmd: CdCmd,
}

#[derive(clap::Subcommand)]
enum CdCmd {
    /// List apps: sync, health, resources.
    Apps {
        #[arg(long)]
        project: Option<String>,
        #[arg(long)]
        cluster: Option<String>,
        /// Synced, OutOfSync or Unknown.
        #[arg(long)]
        sync: Option<String>,
        /// Healthy, Progressing, Degraded, Suspended, Missing or Unknown.
        #[arg(long)]
        health: Option<String>,
        /// Ids containing this.
        #[arg(short = 'q')]
        query: Option<String>,
    },
    /// One app: its status, and with flags its resources, what differs, its history.
    App {
        /// `project/name` (or just `name` in the default project).
        id: String,
        #[arg(long)]
        resources: bool,
        #[arg(long)]
        diff: bool,
        #[arg(long)]
        history: bool,
    },
    /// Sync an app.
    Sync {
        id: String,
        /// Delete what git no longer holds.
        #[arg(long)]
        prune: bool,
        /// Only validate.
        #[arg(long)]
        dry_run: bool,
        /// Argo CD sync options (`--option ServerSideApply=true`).
        #[arg(long = "option")]
        options: Vec<String>,
        /// Wait until it finishes and the app is healthy.
        #[arg(long)]
        wait: bool,
    },
    /// Compare again (`--hard`: render again too).
    Refresh {
        id: String,
        #[arg(long)]
        hard: bool,
    },
    /// Delete an app; its objects too unless `--cascade orphan`.
    Delete {
        id: String,
        /// foreground (default), background or orphan.
        #[arg(long)]
        cascade: Option<String>,
    },
    /// Stop shadowing an app adopted from Argo CD: kiln syncs it from now on.
    TakeOver { id: String },
    /// Roll an app back to an earlier deployment (`kiln cd app <app> --history` lists them; by
    /// default the one before what runs now). It holds there until git changes the app.
    Rollback {
        id: String,
        /// The deployment's id.
        to: Option<u64>,
        /// Delete what that deployment didn't have.
        #[arg(long)]
        prune: bool,
        /// Wait until it finishes and the app is healthy.
        #[arg(long)]
        wait: bool,
    },
    /// Let a rolled-back app follow git again.
    Follow { id: String },
    /// Watch a rollout of kiln's own Deployment, putting the previous image back if it isn't
    /// available in time (run by kiln as a Job before it deploys itself).
    #[command(hide = true)]
    Guard {
        #[arg(long)]
        namespace: String,
        #[arg(long)]
        deployment: String,
        #[arg(long)]
        container: String,
        /// The image to put back.
        #[arg(long)]
        previous: String,
        /// The image being deployed.
        #[arg(long)]
        expect: String,
        /// Seconds the new version has to become available.
        #[arg(long, default_value_t = 600)]
        deadline: u64,
        /// From outside the cluster: a kubeconfig context.
        #[arg(long)]
        context: Option<String>,
        /// The kubeconfig file for `--context` (default: the usual one).
        #[arg(long, default_value = "")]
        kubeconfig: String,
    },
    /// What a commit, or every diff of a stack, would deploy: for each app it touches, its own
    /// change, the stack's change against trunk, and the steps a sync would take against what
    /// runs. Exits non-zero when the plan fails.
    Plan {
        /// A commit (hash, branch or tag), or with `--stack` the stack's top; by default the
        /// branch checked out here, as a stack.
        commit: Option<String>,
        /// Plan every commit from where it leaves trunk (or `--base`) up to COMMIT.
        #[arg(long)]
        stack: bool,
        /// The commit below the stack's bottom.
        #[arg(long)]
        base: Option<String>,
        /// The branch it lands on (default: the repository's default branch).
        #[arg(long)]
        trunk: Option<String>,
        /// A pull request, planned as a stack.
        #[arg(long)]
        pr: Option<u64>,
        /// The repository (default: this checkout's origin, when planning its branch).
        #[arg(long)]
        repo: Option<String>,
        #[arg(long)]
        project: Option<String>,
        #[arg(long)]
        cluster: Option<String>,
        /// Show each object's change.
        #[arg(long)]
        diff: bool,
    },
    /// Import Argo CD manifests (Applications) from files.
    Import { files: Vec<std::path::PathBuf> },
    /// Clusters.
    Cluster {
        #[command(subcommand)]
        cmd: ClusterCmd,
    },
    /// Repository credentials.
    Repo {
        #[command(subcommand)]
        cmd: RepoCmd,
    },
    /// The engine at fleet scale against simulated clusters (no server needed).
    Bench {
        #[arg(long, default_value_t = 10_000)]
        apps: usize,
        #[arg(long, default_value_t = 10)]
        clusters: usize,
        #[arg(long, default_value_t = 3)]
        resources: usize,
        /// Distinct renders (default: every app distinct).
        #[arg(long)]
        distinct: Option<usize>,
        /// Engines in parallel.
        #[arg(long, default_value_t = 1)]
        shards: usize,
        #[arg(long, default_value_t = 0.01)]
        churn: f64,
        #[arg(long, default_value_t = 0.001)]
        drift: f64,
        #[arg(long, default_value_t = 500.0)]
        qps: f64,
        #[arg(long)]
        no_dry_run: bool,
    },
}

#[derive(clap::Subcommand)]
enum ClusterCmd {
    /// Clusters and whether they are connected.
    List,
    /// Add a cluster from a kubeconfig context on the server.
    Add {
        name: String,
        #[arg(long)]
        context: String,
        /// The kubeconfig file (the server's default when unset).
        #[arg(long, default_value = "")]
        kubeconfig: String,
        /// Labels, `key=value`.
        #[arg(long = "label")]
        labels: Vec<String>,
    },
    /// Remove a cluster no app deploys to.
    Rm { name: String },
}

#[derive(clap::Subcommand)]
enum RepoCmd {
    /// Repositories with credentials.
    List,
    /// Add credentials for a repository (or a URL prefix).
    Add {
        url: String,
        #[arg(long)]
        username: Option<String>,
        /// Password or token (read from KILN_REPO_PASSWORD when unset).
        #[arg(long, env = "KILN_REPO_PASSWORD", hide_env_values = true)]
        password: Option<String>,
        #[arg(long)]
        insecure: bool,
    },
    /// Remove a repository's credentials.
    Rm { url: String },
}

struct Api {
    client: Client,
    url: String,
    token: String,
}

impl Api {
    async fn call(&self, method: Method, path: &str, body: Option<Value>) -> Result<Value> {
        self.call_for(method, path, body, Duration::from_secs(60)).await
    }

    async fn call_for(&self, method: Method, path: &str, body: Option<Value>, timeout: Duration) -> Result<Value> {
        let mut req = self.client.request(method, &format!("{}/api/cd{path}", self.url)).bearer(&self.token).timeout(timeout);
        if let Some(b) = body {
            req = req.json(&b);
        }
        let resp = req.send().await?;
        let status = resp.status();
        let bytes = resp.bytes(64 << 20).await?;
        let v: Value = if bytes.is_empty() { Value::Null } else { serde_json::from_slice(&bytes).unwrap_or(Value::String(String::from_utf8_lossy(&bytes).into_owned())) };
        if !(200..300).contains(&status) {
            bail!("{}", v.get("error").and_then(Value::as_str).map(str::to_owned).unwrap_or_else(|| format!("HTTP {status}")));
        }
        Ok(v)
    }
}

fn app_path(id: &str) -> String {
    let id = if id.contains('/') { id.to_owned() } else { format!("default/{id}") };
    format!("/apps/{}", id.split('/').map(kiln_cd::kube::enc).collect::<Vec<_>>().join("/"))
}

fn s<'a>(v: &'a Value, k: &str) -> &'a str {
    v.get(k).and_then(Value::as_str).unwrap_or("")
}

/// Runs `kiln cd`.
pub async fn run(args: CdArgs, cfg: &Config) -> Result<()> {
    if let CdCmd::Bench { apps, clusters, resources, distinct, shards, churn, drift, qps, no_dry_run } = args.cmd {
        let b = kiln_cd::bench::Bench {
            apps,
            clusters,
            resources: resources.max(2),
            distinct: distinct.unwrap_or(apps).max(1),
            churn,
            drift,
            qps,
            dry_run: !no_dry_run,
            shards,
            ..kiln_cd::bench::Bench::default()
        };
        let report = crate::rt::blocking(move || {
            let r = kiln_cd::bench::run(&b, |p| {
                eprintln!("{:<20} simulated {:>8.1} s · wall {:>7.2} s · engine {:>7.2} s", p.name, p.sim_ms as f64 / 1000.0, p.wall_ms as f64 / 1000.0, p.engine_ms as f64 / 1000.0)
            });
            kiln_cd::bench::table(&b, &r)
        })
        .await?;
        println!("{report}");
        return Ok(());
    }
    if let CdCmd::Guard { namespace, deployment, container, previous, expect, deadline, context, kubeconfig } = args.cmd {
        let a = super::guard::Args { namespace, deployment, container, previous, expect, deadline: std::time::Duration::from_secs(deadline), context: context.map(|c| (c, kubeconfig)) };
        return super::guard::run(&a).await;
    }
    let t = crate::access::resolve(cfg, args.url.as_deref(), args.token.as_deref()).await?;
    let api = Api { client: Client::public()?, url: t.url, token: t.token };
    let out = match args.cmd {
        CdCmd::Apps { project, cluster, sync, health, query } => {
            let mut q = vec![];
            for (k, v) in [("project", project), ("cluster", cluster), ("sync", sync), ("health", health), ("q", query)] {
                if let Some(v) = v {
                    q.push(format!("{k}={}", kiln_cd::kube::enc(&v)));
                }
            }
            q.push("limit=1000".into());
            let v = api.call(Method::Get, &format!("/apps?{}", q.join("&")), None).await?;
            if !args.json {
                let apps = v.get("apps").and_then(Value::as_array).cloned().unwrap_or_default();
                if apps.is_empty() {
                    println!("No apps.");
                }
                for a in &apps {
                    let differ = a.get("out_of_sync").and_then(Value::as_u64).unwrap_or(0);
                    let total = a.get("resources").and_then(Value::as_u64).unwrap_or(0);
                    let mut flags = String::new();
                    if a.get("shadow").and_then(Value::as_bool) == Some(true) {
                        flags.push_str(" shadow");
                    }
                    if let Some(e) = a.get("source_error").and_then(Value::as_str) {
                        flags.push_str(&format!(" source error: {e}"));
                    }
                    println!("{:<40} {:<10} {:<11} {:>4}/{:<4} {}{}", s(a, "app"), s(a, "sync"), s(a, "health"), total - differ.min(total), total, s(a, "cluster"), flags);
                }
                return Ok(());
            }
            v
        }
        CdCmd::App { id, resources, diff, history } => {
            let path = app_path(&id);
            let mut v = api.call(Method::Get, &path, None).await?;
            if resources {
                let r = api.call(Method::Get, &format!("{path}/resources"), None).await?;
                if let Some(m) = v.as_object_mut() {
                    m.insert("resources".into(), r.get("resources").cloned().unwrap_or(Value::Null));
                }
            }
            if diff {
                let d = api.call(Method::Get, &format!("{path}/diff"), None).await?;
                if !args.json {
                    for x in d.get("diffs").and_then(Value::as_array).into_iter().flatten() {
                        let k = x.get("key").cloned().unwrap_or(Value::Null);
                        println!("--- {}/{} {}/{}: {}", s(&k, "group"), s(&k, "kind"), s(&k, "namespace"), s(&k, "name"), s(x, "why"));
                        for h in x.get("hunks").and_then(Value::as_array).into_iter().flatten() {
                            for l in h.get("lines").and_then(Value::as_array).into_iter().flatten().filter_map(Value::as_str) {
                                println!("{l}");
                            }
                        }
                    }
                    return Ok(());
                }
                if let Some(m) = v.as_object_mut() {
                    m.insert("diffs".into(), d.get("diffs").cloned().unwrap_or(Value::Null));
                }
            }
            if history {
                let h = api.call(Method::Get, &format!("{path}/history"), None).await?;
                if let Some(m) = v.as_object_mut() {
                    m.insert("history".into(), h.get("history").cloned().unwrap_or(Value::Null));
                }
            }
            v
        }
        CdCmd::Sync { id, prune, dry_run, options, wait } => {
            let path = app_path(&id);
            let before = op_id(&api, &path).await;
            let v = api.call(Method::Post, &format!("{path}/sync"), Some(json!({"prune": prune, "dry_run": dry_run, "options": options}))).await?;
            if wait {
                return wait_for(&api, &path, args.json, before).await;
            }
            v
        }
        CdCmd::Rollback { id, to, prune, wait } => {
            let path = app_path(&id);
            let before = op_id(&api, &path).await;
            let v = api.call(Method::Post, &format!("{path}/rollback"), Some(json!({"to": to, "prune": prune}))).await?;
            if wait {
                return wait_for(&api, &path, args.json, before).await;
            }
            v
        }
        CdCmd::Follow { id } => api.call(Method::Post, &format!("{}/follow", app_path(&id)), None).await?,
        CdCmd::Plan { commit, stack, base, trunk, pr, repo, project, cluster, diff } => {
            if commit.as_deref().is_some_and(|c| c.len() > 1 && c.starts_with('D') && c[1..].chars().all(|x| x.is_ascii_digit())) {
                bail!("plans of kiln's own diffs (D123) come with its review, which isn't built yet: plan the commit or the stack (--stack)");
            }
            // By default, the branch checked out here, as a stack, from this checkout's origin.
            let here = commit.is_none() && pr.is_none();
            let (head, repo) = match (commit, here) {
                (Some(c), _) => (Some(c), repo),
                (None, true) => {
                    let branch = local_git(&["rev-parse", "--abbrev-ref", "HEAD"]).context("say which commit to plan (not in a git checkout)")?;
                    let origin = match repo {
                        Some(r) => Some(r),
                        None => local_git(&["remote", "get-url", "origin"]).ok(),
                    };
                    (Some(branch), origin)
                }
                (None, false) => (None, repo),
            };
            let as_stack = stack || here;
            let mut body = json!({"repo": repo, "base": base, "trunk": trunk, "pr": pr, "project": project, "cluster": cluster, "brief": !diff});
            if let (Some(h), Some(m)) = (head, body.as_object_mut()) {
                m.insert(if as_stack { "head" } else { "commit" }.into(), json!(h));
            }
            if let Some(m) = body.as_object_mut() {
                m.retain(|_, v| !v.is_null());
            }
            let v = api.call_for(Method::Post, "/plans", Some(body), Duration::from_secs(600)).await.map_err(|e| {
                if here { e.context("planning the branch checked out here (pushed?)") } else { e }
            })?;
            if !args.json {
                print_plan(&v, diff);
            }
            if v.get("ok") != Some(&Value::Bool(true)) {
                if args.json {
                    println!("{}", serde_json::to_string_pretty(&v).unwrap_or_default());
                }
                bail!("the plan fails");
            }
            if !args.json {
                return Ok(());
            }
            v
        }
        CdCmd::Refresh { id, hard } => api.call(Method::Post, &format!("{}/refresh?hard={hard}", app_path(&id)), None).await?,
        CdCmd::Delete { id, cascade } => {
            let q = cascade.map(|c| format!("?cascade={c}")).unwrap_or_default();
            api.call(Method::Delete, &format!("{}{q}", app_path(&id)), None).await?
        }
        CdCmd::TakeOver { id } => api.call(Method::Post, &format!("{}/take-over", app_path(&id)), None).await?,
        CdCmd::Import { files } => {
            let mut all = String::new();
            for f in files {
                all.push_str(&std::fs::read_to_string(&f).with_context(|| format!("reading {}", f.display()))?);
                all.push_str("\n---\n");
            }
            let resp = api.client.request(Method::Post, &format!("{}/api/cd/import", api.url)).bearer(&api.token).body(all).send().await?;
            let ok = resp.is_success();
            let v: Value = resp.json(8 << 20).await?;
            if !ok {
                bail!("{}", s(&v, "error"));
            }
            v
        }
        CdCmd::Cluster { cmd: ClusterCmd::List } => api.call(Method::Get, "/clusters", None).await?,
        CdCmd::Cluster { cmd: ClusterCmd::Add { name, context, kubeconfig, labels } } => {
            let labels: serde_json::Map<String, Value> =
                labels.iter().filter_map(|l| l.split_once('=')).map(|(k, v)| (k.to_owned(), Value::String(v.to_owned()))).collect();
            api.call(Method::Post, "/clusters", Some(json!({"name": name, "context": context, "kubeconfig": kubeconfig, "labels": labels}))).await?
        }
        CdCmd::Cluster { cmd: ClusterCmd::Rm { name } } => api.call(Method::Delete, &format!("/clusters/{}", kiln_cd::kube::enc(&name)), None).await?,
        CdCmd::Repo { cmd: RepoCmd::List } => api.call(Method::Get, "/repos", None).await?,
        CdCmd::Repo { cmd: RepoCmd::Add { url, username, password, insecure } } => {
            api.call(Method::Post, "/repos", Some(json!({"url": url, "kind": "Git", "username": username, "password": password, "insecure": insecure}))).await?
        }
        CdCmd::Repo { cmd: RepoCmd::Rm { url } } => api.call(Method::Delete, &format!("/repos?url={}", kiln_cd::kube::enc(&url)), None).await?,
        CdCmd::Bench { .. } | CdCmd::Guard { .. } => Value::Null,
    };
    if !out.is_null() {
        println!("{}", serde_json::to_string_pretty(&out).unwrap_or_default());
    }
    Ok(())
}

/// A git command in the current directory, its output trimmed.
fn local_git(args: &[&str]) -> Result<String> {
    let out = std::process::Command::new("git").args(args).output().context("running git")?;
    if !out.status.success() {
        bail!("{}", String::from_utf8_lossy(&out.stderr).trim());
    }
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_owned())
}

fn key_text(k: &Value) -> String {
    let (g, kind, ns, n) = (s(k, "group"), s(k, "kind"), s(k, "namespace"), s(k, "name"));
    let kind = if g.is_empty() { kind.to_owned() } else { format!("{g}/{kind}") };
    if ns.is_empty() { format!("{kind} {n}") } else { format!("{kind} {ns}/{n}") }
}

fn count(v: &Value, k: &str) -> u64 {
    v.get(k).and_then(Value::as_u64).unwrap_or(0)
}

fn hunks_text(x: &Value) {
    for h in x.get("hunks").and_then(Value::as_array).into_iter().flatten() {
        for l in h.get("lines").and_then(Value::as_array).into_iter().flatten().filter_map(Value::as_str) {
            println!("        {l}");
        }
    }
}

/// A plan for a terminal: each diff, the apps it changes, and what a sync of each would do.
fn print_plan(v: &Value, diff: bool) {
    let diffs = v.get("diffs").and_then(Value::as_array).cloned().unwrap_or_default();
    let trunk = v.get("trunk").cloned().unwrap_or(Value::Null);
    let short = |sha: &str| sha.chars().take(10).collect::<String>();
    println!(
        "{} {} on {} ({}){}",
        if v.get("ok") == Some(&Value::Bool(true)) { "ok:" } else { "FAILS:" },
        if diffs.len() == 1 { "1 commit".to_owned() } else { format!("{} commits", diffs.len()) },
        s(&trunk, "branch"),
        short(s(&trunk, "commit")),
        v.get("repo").and_then(Value::as_str).map(|r| format!(" in {r}")).unwrap_or_default(),
    );
    for d in &diffs {
        let c = d.get("commit").cloned().unwrap_or(Value::Null);
        println!("\n{} {}  {}", short(s(&c, "sha")), s(&c, "subject"), s(&c, "author"));
        let apps = d.get("apps").and_then(Value::as_array).cloned().unwrap_or_default();
        if apps.is_empty() {
            println!("  changes no app");
        }
        for a in &apps {
            let ok = a.get("ok") == Some(&Value::Bool(true));
            let mut how = Vec::new();
            if a.get("shadow") == Some(&Value::Bool(true)) {
                how.push("shadowed: Argo CD deploys it");
            } else if a.get("auto") == Some(&Value::Bool(true)) {
                how.push(if a.get("auto_prune") == Some(&Value::Bool(true)) { "syncs on landing, prunes" } else { "syncs on landing" });
            } else {
                how.push("synced by hand");
            }
            let changes = |k: &str| a.get(k).filter(|x| !x.is_null()).map(|x| format!("+{} ~{} -{}", count(x, "added"), count(x, "changed"), count(x, "removed")));
            // An app a lower diff changed and this one leaves alone: one line.
            if ok && a.get("touched") == Some(&Value::Bool(false)) {
                println!("  · {}  unchanged by this diff; stack {}", s(a, "app"), changes("stack").unwrap_or_default());
                continue;
            }
            let mut line = format!("  {} {}", if ok { "✓" } else { "✗" }, s(a, "app"));
            if let Some(own) = changes("own") {
                line.push_str(&format!("  own {own}"));
            }
            if let Some(stack) = changes("stack") {
                line.push_str(&format!("  stack {stack}"));
            }
            println!("{line}  ({})", how.join(", "));
            if let Some(e) = a.get("error").and_then(Value::as_str) {
                println!("      {e}");
            }
            for p in a.get("problems").and_then(Value::as_array).into_iter().flatten().filter_map(Value::as_str) {
                if Some(p) != a.get("error").and_then(Value::as_str) {
                    println!("      problem: {p}");
                }
            }
            for n in a.get("notes").and_then(Value::as_array).into_iter().flatten().filter_map(Value::as_str) {
                println!("      note: {n}");
            }
            if diff {
                for o in a.pointer("/own/objects").and_then(Value::as_array).into_iter().flatten() {
                    println!("      {} {}", s(o, "change"), key_text(o.get("key").unwrap_or(&Value::Null)));
                    hunks_text(o);
                }
            }
            let sync = a.get("sync").cloned().unwrap_or(Value::Null);
            if sync.is_null() {
                continue;
            }
            let steps = sync.get("steps").and_then(Value::as_array).cloned().unwrap_or_default();
            if steps.is_empty() {
                println!("      a sync changes nothing in {}", s(a, "cluster"));
            } else {
                println!("      a sync in {}: create {}, update {}, prune {}, hooks {}", s(a, "cluster"), count(&sync, "create"), count(&sync, "update"), count(&sync, "prune"), count(&sync, "hooks"));
            }
            for st in &steps {
                let why = st.get("why").and_then(Value::as_str).map(|w| format!("  {w}")).unwrap_or_default();
                println!("        {:<6} {:<8} wave {:<3} {}{why}", s(st, "step"), s(st, "phase"), st.get("wave").and_then(Value::as_i64).unwrap_or(0), key_text(st.get("key").unwrap_or(&Value::Null)));
            }
        }
    }
    let apps = v.get("apps").cloned().unwrap_or(Value::Null);
    let n = |k: &str| match count(&apps, k) {
        1 => "1 app".to_owned(),
        n => format!("{n} apps"),
    };
    let others = match count(&apps, "unaffected") {
        0 => String::new(),
        1 => format!(" 1 other app follows {} and isn't touched.", s(&trunk, "branch")),
        k => format!(" {k} other apps follow {} and aren't touched.", s(&trunk, "branch")),
    };
    println!("\n{} touched, {} planned.{others}", n("affected"), count(&apps, "planned"));
    for n in v.get("notes").and_then(Value::as_array).into_iter().flatten().filter_map(Value::as_str) {
        println!("note: {n}");
    }
}

/// The id of an app's current (or last) sync, if it has one.
async fn op_id(api: &Api, path: &str) -> Option<u64> {
    api.call(Method::Get, path, None).await.ok()?.pointer("/operation/id").and_then(Value::as_u64)
}

/// Waits for the sync after `before` to finish with the app healthy.
async fn wait_for(api: &Api, path: &str, json_out: bool, before: Option<u64>) -> Result<()> {
    let mut last = String::new();
    for _ in 0..1200 {
        let v = api.call(Method::Get, path, None).await?;
        // Until the new sync shows, the last one's outcome says nothing.
        let fresh = v.pointer("/operation/id").and_then(Value::as_u64) != before;
        let phase = if fresh { v.pointer("/operation/phase").and_then(Value::as_str).unwrap_or("") } else { "" };
        let line = format!("{} · {} · {} {}", s(&v, "sync"), s(&v, "health"), phase, v.pointer("/operation/message").and_then(Value::as_str).unwrap_or(""));
        if line != last && !json_out {
            println!("{line}");
            last = line;
        }
        match phase {
            "Failed" => bail!("the sync failed"),
            "Succeeded" if s(&v, "health") == "Healthy" && s(&v, "sync") == "Synced" => {
                if json_out {
                    println!("{}", serde_json::to_string_pretty(&v).unwrap_or_default());
                }
                return Ok(());
            }
            _ => {}
        }
        crate::rt::sleep(Duration::from_millis(500)).await;
    }
    bail!("gave up waiting after 10 minutes")
}
