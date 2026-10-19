//! Deploying by API (docs/deploy-api.md): apps, environments, registrations and releases. A
//! release is a commit to an environment's release repository, which the environment's CD
//! provider turns into what runs; this module asks for the commit, makes it, hands it to the
//! provider and follows it until it has landed or failed, across restarts.
//!
//! - [`model`]: the records, and `release.json`.
//! - [`store`]: where the records are kept (kiln's store, until KilnDB).
//! - [`repo`]: the release repository's layout.
//! - [`host`]: kiln's own git server.
//! - [`kilncd`], [`argocd`]: the CD providers, KilnCD and Argo CD; [`provider`]: what they answer.
//! - [`check`]: what a release is checked for before it is recorded.
//! - [`flow`]: releases, from request to landed, and the worker that follows them.
//! - [`api`], [`cli`], [`ops`]: `/api/deploy`, `kiln deploy` and `kiln app`, and the `deploy.*`
//!   operations agents reach through MCP and code mode.

pub mod api;
pub mod argocd;
pub mod check;
pub mod cli;
pub mod flow;
pub mod host;
pub mod kilncd;
pub mod model;
pub mod ops;
pub mod provider;
pub mod repo;
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
}

static DEPLOY: RwLock<Option<Arc<Deploy>>> = RwLock::new(None);

/// The service, once it has started.
pub fn get() -> Option<Arc<Deploy>> {
    DEPLOY.read().clone()
}

/// Runs the service until the server shuts down: the worker that follows releases.
pub async fn run(hub: Arc<Hub>) -> anyhow::Result<()> {
    let d = Arc::new(Deploy {
        hosted: host::Hosted::new(&hub.cfg.server.data_dir),
        owner: format!("server-{}", crate::util::new_id()),
        wake: crate::rt::Notify::new(),
        changes: crate::rt::Broadcast::new(256),
        hub: Arc::clone(&hub),
    });
    *DEPLOY.write() = Some(Arc::clone(&d));
    let result = flow::work(&d).await;
    *DEPLOY.write() = None;
    result
}
