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
pub mod sources;
pub mod store;

use std::sync::Arc;

use parking_lot::RwLock;

use crate::hub::Hub;

/// The running service.
pub struct Deploy {
    pub hub: Arc<Hub>,
    /// Its records (§14.1), through the Records seam: the hub's, or in a test another's, so two
    /// servers can share them.
    pub records: Arc<dyn kiln_seams::Records>,
    /// In a test, what stands in for its providers (KilnCD, Argo CD).
    #[cfg(test)]
    pub stand_in: Option<Arc<dyn provider::StandIn>>,
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

/// Why the service couldn't start, the last time it tried.
static UNSTARTED: RwLock<Option<String>> = RwLock::new(None);

/// The service, once it has started.
pub fn get() -> Option<Arc<Deploy>> {
    DEPLOY.read().clone()
}

/// What a call is answered while the service isn't running: why it couldn't start, when it tried.
pub fn not_running() -> String {
    match &*UNSTARTED.read() {
        Some(why) => format!("deploying by API hasn't started here yet: {why}. It starts once that's done"),
        None => "deploying by API isn't running here (it runs with [cd] enabled)".to_owned(),
    }
}

/// Runs the service until the server shuts down: the worker that follows releases.
pub async fn run(hub: Arc<Hub>) -> anyhow::Result<()> {
    let hosted = host::Hosted::new(&crate::ci::hosting::root(&hub));
    // Release repositories from before they were native ones move into kiln's hosted root.
    for repo in hosted.adopt(&hub.cfg.server.data_dir.join("deploy").join("repos")) {
        tracing::info!(%repo, "deploy: a release repository moved into kiln's hosted root");
    }
    // What kiln's store held for deploy before its records moved onto the seam, copied once. Deploy
    // doesn't start without it: a release made first would write the counter row, and the copy
    // would never be made. Its supervisor starts it again, backing off, until the copy is made.
    let copied = match store::adopt(&*hub.records, &hub.store).await {
        Ok(n) => n,
        Err(e) => {
            let e = e.context("its records couldn't be copied from kiln's tables onto the Records seam");
            *UNSTARTED.write() = Some(format!("{e:#}"));
            return Err(e);
        }
    };
    *UNSTARTED.write() = None;
    if copied > 0 {
        tracing::info!(rows = copied, "deploy: its records copied from kiln's tables onto the Records seam, the tables left as they were");
    }
    let d = Arc::new(Deploy {
        records: Arc::clone(&hub.records),
        #[cfg(test)]
        stand_in: None,
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
    match store::environments(&*d.records).await {
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

#[cfg(test)]
mod tests {
    use super::*;

    /// Deploy doesn't start while what kiln's tables held can't be copied onto the seam, so no
    /// release can write the counter row before the copy; its supervisor starts it again, and the
    /// start after the copy can be made makes it.
    #[test]
    fn deploy_doesn_t_start_until_kiln_s_tables_are_copied() -> anyhow::Result<()> {
        crate::rt::block_on(async {
            let hub = crate::hub::tests::scratch_hub().await?;
            // A release the copy can't read.
            hub.store
                .call(|c| {
                    c.execute("INSERT INTO deploy_releases (id, app, environment, state, spec, created_at) VALUES (3, 'web', 'staging', 'succeeded', 'not a release', 0)", [])?;
                    Ok(())
                })
                .await?;
            let Err(e) = Box::pin(run(Arc::clone(&hub))).await else { anyhow::bail!("deploy started without its copy") };
            anyhow::ensure!(format!("{e:#}").contains("couldn't be copied"), "{e:#}");
            anyhow::ensure!(not_running().contains("couldn't be copied"), "calls aren't told why: {}", not_running());
            anyhow::ensure!(store::release(&*hub.records, 3).await?.is_none(), "a copy cut short left a release");
            // Readable again, it's copied at the next start.
            hub.store
                .call(|c| {
                    c.execute(r#"UPDATE deploy_releases SET spec = '{"id":3,"app":"web","environment":"staging","kind":"deploy","state":"succeeded","requested_by":"ada","created_at":0}' WHERE id = 3"#, [])?;
                    Ok(())
                })
                .await?;
            anyhow::ensure!(store::adopt(&*hub.records, &hub.store).await? > 0, "the copy wasn't made");
            anyhow::ensure!(store::release(&*hub.records, 3).await?.is_some_and(|r| r.app == "web"), "release 3 wasn't copied");
            anyhow::ensure!(store::adopt(&*hub.records, &hub.store).await? == 0, "the copy was made twice");
            Ok(())
        })
    }
}
