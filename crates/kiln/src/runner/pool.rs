//! A pool's executors on this runner (docs/ci.md W13-R09): started when the server asks, ready
//! before a leg comes, each taking one leg and then discarded, so every leg starts clean.
//!
//! A `docker` executor is a started container of the pool's image, laid out as the `docker`
//! executor lays out a leg's (W13-R04) under a directory of its own (`<work>/p/<id>`), so a leg
//! that comes to it runs there at once. A leg it can't take as it is (one with `container:`,
//! `services:` or a contained network) runs the usual way, and the executor is discarded all the
//! same. The other kinds (`firecracker`, `shards`, `k8s`) are refused until this runner has them.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use serde::{Deserialize, Serialize};

use super::docker::{self, Cli, Containers, PathMap, Spec};

/// What the server asks (`ServerMsg::Executor`).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Ask {
    /// `start` or `stop`.
    pub op: String,
    pub executor: String,
    pub pool: String,
    pub kind: String,
    pub image: Option<String>,
    /// kiln's registry, and a token that pulls the image from it.
    pub registry: Option<String>,
    pub pull_token: Option<String>,
}

/// What this runner says of one (`AgentMsg::Executor`).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Said {
    pub executor: String,
    /// `ready` or `failed`.
    pub state: String,
    pub error: Option<String>,
}

/// One ready executor.
pub struct Warm {
    pub id: String,
    pub pool: String,
    pub image: String,
    /// Its directory: the leg's runner workspace when it runs here.
    pub dir: PathBuf,
    pub containers: Containers,
    /// The tools' directory it mounted, leased as a leg leases one (the leg takes it over).
    pub tools: Option<super::workspaces::ToolCache>,
}

/// This runner's ready executors.
#[derive(Clone, Default)]
pub struct Executors(Arc<parking_lot::Mutex<HashMap<String, Warm>>>);

impl Executors {
    pub fn take(&self, id: &str) -> Option<Warm> {
        self.0.lock().remove(id)
    }

    fn put(&self, w: Warm) {
        self.0.lock().insert(w.id.clone(), w);
    }

    /// Takes them all at once: the server forgot them (a new connection), so they're discarded.
    pub fn drain(&self) -> Vec<Warm> {
        self.0.lock().drain().map(|(_, w)| w).collect()
    }
}

/// Removes what executors of an earlier process left (`<work>/p`): this one has none yet.
pub fn sweep(work: &Path) {
    let _ = std::fs::remove_dir_all(work.join("p"));
}

/// Where a leg runs in executor `id`: `<work>/p/<id>`.
pub fn dir_of(work: &Path, id: &str) -> PathBuf {
    work.join("p").join(id.chars().map(|c| if c.is_ascii_alphanumeric() || c == '-' { c } else { '_' }).collect::<String>())
}

/// The tools' directory of the `docker` executor's image, as a leg's layout has it.
pub fn tool_cache(work: &Path, image: &str) -> PathBuf {
    work.join("_tool").join(image.chars().map(|c| if c.is_ascii_alphanumeric() || c == '.' { c } else { '-' }).collect::<String>())
}

/// Does what the server asked; what to tell it (`None` for a stop).
pub async fn handle(execs: &Executors, work: &Path, ask: Ask) -> Option<Said> {
    match ask.op.as_str() {
        "stop" => {
            if let Some(w) = execs.take(&ask.executor) {
                discard(w).await;
            }
            None
        }
        "start" => Some(match start(work, &ask).await {
            Ok(w) => {
                let said = Said { executor: w.id.clone(), state: "ready".into(), error: None };
                execs.put(w);
                said
            }
            Err(e) => Said { executor: ask.executor.clone(), state: "failed".into(), error: Some(format!("{e:#}")) },
        }),
        other => Some(Said { executor: ask.executor.clone(), state: "failed".into(), error: Some(format!("`{other}`: start or stop")) }),
    }
}

async fn start(work: &Path, ask: &Ask) -> anyhow::Result<Warm> {
    anyhow::ensure!(ask.kind == "docker", "this runner starts `docker` pools' executors; `{}` comes with its executor (docs/ci.md W13-R05 to R07)", ask.kind);
    let image = ask.image.clone().filter(|i| !i.trim().is_empty()).ok_or_else(|| anyhow::anyhow!("the pool names no image"))?;
    let bin = crate::rt::blocking(docker::find).await?.ok_or_else(|| anyhow::anyhow!("this runner has no working Docker or Podman"))?;
    let dir = dir_of(work, &ask.executor);
    let temp = dir.join("_temp");
    let config = temp.join("_docker_config");
    // Leased as a leg of the `docker` executor leases its image's tools, which the leg takes over.
    let lease = super::workspaces::ToolCache::take(&tool_cache(work, &image));
    let tools = lease.path();
    let dirs = [config.clone(), temp.join("_github_home"), temp.join("_github_workflow"), work.join("_actions"), tools.clone(), work.join("_externals")];
    crate::rt::blocking(move || dirs.iter().try_for_each(std::fs::create_dir_all)).await??;
    // Pulls as the runner's own Docker would, and from kiln's registry with the token the server
    // gave for this image.
    let mut conf = super::credential::runner_config().unwrap_or_else(|| serde_json::json!({}));
    if let (Some(host), Some(token), Some(o)) = (ask.registry.as_deref().map(crate::githost::host_of), ask.pull_token.as_deref(), conf.as_object_mut()) {
        let auths = o.entry("auths").or_insert_with(|| serde_json::json!({}));
        if let Some(a) = auths.as_object_mut() {
            a.insert(host, serde_json::json!({ "registrytoken": token }));
        }
    }
    super::auth::write_private(&config.join("config.json"), conf.to_string().as_bytes())?;
    // The paths a leg's layout will have, at the same paths inside (the `docker` executor's).
    let same = |p: &Path| p.to_string_lossy().into_owned();
    let map = PathMap::new(vec![
        (dir.clone(), same(&dir)),
        (temp.join("_github_home"), "/github/home".into()),
        (temp.join("_github_workflow"), "/github/workflow".into()),
        (work.join("_actions"), same(&work.join("_actions"))),
        (tools, "/opt/hostedtoolcache".into()),
        (work.join("_externals"), "/__e".into()),
    ]);
    let cli = Cli::new(&bin, config).owned_by(work);
    let spec = Spec { image: image.clone(), user: docker::host_user(), ..Spec::default() };
    let said = parking_lot::Mutex::new(Vec::<String>::new());
    let containers = docker::Containers::start(cli, &ask.executor, map, &same(&dir), Some(spec), Vec::new(), None, &|m: String| said.lock().push(m)).await?;
    tracing::info!(executor = %ask.executor, pool = %ask.pool, image = %image, "pool executor ready");
    Ok(Warm { id: ask.executor.clone(), pool: ask.pool.clone(), image, dir, containers, tools: Some(lease) })
}

/// Removes an executor: its containers and its directory.
pub async fn discard(w: Warm) {
    w.containers.stop().await;
    let dir = w.dir.clone();
    let _ = crate::rt::blocking(move || std::fs::remove_dir_all(dir)).await;
}

/// Whether a leg can run in a ready executor as it is: the `docker` executor's own container,
/// of the pool's image, with nothing the executor wasn't started with.
pub fn adoptable(a: &kiln_ci::wire::AssignmentV2, w: &Warm) -> bool {
    a.executor.kind == "docker" && a.executor.image.as_deref() == Some(w.image.as_str()) && a.job_container.is_none() && a.job_services.is_empty() && a.network.is_none()
}
