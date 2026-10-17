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
        let mut req = self.client.request(method, &format!("{}/api/cd{path}", self.url)).bearer(&self.token).timeout(Duration::from_secs(60));
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
            let v = api.call(Method::Post, &format!("{path}/sync"), Some(json!({"prune": prune, "dry_run": dry_run, "options": options}))).await?;
            if wait {
                return wait_for(&api, &path, args.json).await;
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
        CdCmd::Bench { .. } => Value::Null,
    };
    if !out.is_null() {
        println!("{}", serde_json::to_string_pretty(&out).unwrap_or_default());
    }
    Ok(())
}

/// Follows an app until its sync finishes and it is healthy (or the sync fails).
async fn wait_for(api: &Api, path: &str, json_out: bool) -> Result<()> {
    let mut last = String::new();
    for _ in 0..1200 {
        let v = api.call(Method::Get, path, None).await?;
        let phase = v.pointer("/operation/phase").and_then(Value::as_str).unwrap_or("");
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
