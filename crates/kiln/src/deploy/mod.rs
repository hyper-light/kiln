//! Deploying by API (docs/deploy-api.md): apps, environments, registrations and releases. A
//! release is a commit to an environment's release repository, which the environment's CD
//! provider turns into what runs; this module asks for the commit, makes it, hands it to the
//! provider and follows it until it has landed or failed, across restarts.
//!
//! - [`model`]: the records, and `release.json`.
//! - [`store`]: where the records are kept (kiln's store, until KilnDB).
//! - [`repo`]: the release repository's layout.
//! - [`git`]: an environment's release repository, wherever it is: [`host`], kiln's own git
//!   server, or [`remote`], GitHub through kiln's GitHub App, a GitLab with its token or Bitbucket
//!   (their pull and merge requests, deployments and webhooks too).
//! - [`kilncd`], [`argocd`]: the CD providers, KilnCD and Argo CD ([`appset`]: an environment's
//!   ApplicationSet there); [`provider`]: what they answer; [`moves`]: an environment moved from one
//!   to the other.
//! - [`check`]: what a release is checked for before it is recorded; [`gates`]: what it waits for
//!   before its commit ([`external`]: the checks that ask a URL or run a workflow; [`link`]: linked
//!   kiln servers, whose signed statements prove an `after`).
//! - [`flow`]: releases, from request to landed, and the worker that follows them.
//! - [`scaffold`]: an app's source repository, made from its template's starter; [`init`]: its
//!   chart on kiln's base chart (`kiln app init`).
//! - [`api`], [`cli`], [`ops`]: `/api/deploy`, `kiln deploy` and `kiln app`, and the `deploy.*`
//!   operations agents reach through MCP and code mode.

pub mod actions;
pub mod api;
pub mod appset;
pub mod argocd;
pub mod check;
pub mod cli;
pub mod external;
pub mod flow;
pub mod gates;
pub mod git;
pub mod host;
pub mod init;
pub mod kilncd;
pub mod link;
pub mod metrics;
pub mod model;
pub mod moves;
pub mod ops;
pub mod preflight;
pub mod provider;
pub mod reads;
pub mod remote;
pub mod repo;
pub mod argo_secrets;
pub mod registries;
pub mod scaffold;
pub mod store;

use std::sync::Arc;

use parking_lot::RwLock;

use crate::hub::Hub;

/// The running service.
pub struct Deploy {
    pub hub: Arc<Hub>,
    /// kiln's own git server.
    pub hosted: host::Hosted,
    /// This server's name on the leases it takes.
    pub owner: String,
    /// A release asked for, or moved: the worker looks at once rather than at its next tick.
    pub wake: crate::rt::Notify,
    /// Each release that moved, by id, for whoever waits on one.
    pub changes: crate::rt::Broadcast<i64>,
    /// What each Argo CD's stream says of the Applications kiln made.
    pub argo: argocd::Watches,
    /// Release repositories on GitHub, a GitLab or Bitbucket: their mirrors, and the releases
    /// reported there.
    pub remote: Arc<remote::Mirrors>,
}

static DEPLOY: RwLock<Option<Arc<Deploy>>> = RwLock::new(None);

/// The service, once it has started.
pub fn get() -> Option<Arc<Deploy>> {
    DEPLOY.read().clone()
}

/// Runs the service until the server shuts down: the worker that follows releases.
pub async fn run(hub: Arc<Hub>) -> anyhow::Result<()> {
    let hosted = host::Hosted::new(&crate::ci::hosting::root(&hub));
    // Release repositories from before they were native ones move into kiln's hosted root.
    for repo in hosted.adopt(&hub.cfg.server.data_dir.join("deploy").join("repos")) {
        tracing::info!(%repo, "deploy: a release repository moved into kiln's hosted root");
    }
    let d = Arc::new(Deploy {
        hosted,
        owner: format!("server-{}", crate::util::new_id()),
        wake: crate::rt::Notify::new(),
        changes: crate::rt::Broadcast::new(256),
        argo: argocd::Watches::default(),
        remote: Arc::default(),
        hub: Arc::clone(&hub),
    });
    *DEPLOY.write() = Some(Arc::clone(&d));
    // Each environment's release repository is a native repository, with its row.
    match store::environments(&hub.store).await {
        Ok(envs) => {
            for e in envs.iter().filter(|e| e.release_repo.host == model::KILN_HOST) {
                if let Err(why) = flow::ensure_release_repo(&d, &e.release_repo).await {
                    tracing::warn!(environment = %e.name, repo = %e.release_repo.repo, error = %why, "deploy: an environment's release repository isn't one of kiln's own");
                }
            }
        }
        Err(e) => tracing::warn!(error = %format!("{e:#}"), "deploy: environments can't be read"),
    }
    // Moves between providers under way go on (§8.4).
    moves::resume(&d).await;
    let result = flow::work(&d).await;
    *DEPLOY.write() = None;
    result
}
