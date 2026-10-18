//! Images kiln publishes (docs/cd-targets.md §7.1, §10): an app's artifact can be
//! `published: <name>`, an image a pipeline builds at each commit. Where it lives is a setting
//! people change in the UI (Settings → Deployments → Images), not in git:
//!
//! - **Local** (the default): the local Docker of the machine that builds it. The pipeline loads it
//!   into the cluster's nodes, as `deploy/cluster/load-image.sh` does, and a revision's image is
//!   ready once that commit's pipeline run succeeded. The cluster runs it by its tag
//!   (`kiln:<commit>`), as it is on the nodes.
//! - **A registry**: `docker.io/acme/kiln`, say. The pipeline pushes it there by commit, and kiln
//!   resolves the commit's tag to its digest, as for any image.
//!
//! The pipeline learns where to put it from its job's environment: `KILN_IMAGE_<NAME>` is the
//! repository, or `local`.

use std::collections::BTreeMap;

use anyhow::{Result, bail};
use kiln_cd::apps::{AppDef, ArtifactSpec, Origin};
use kiln_cd::state::Command;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::Cd;
use crate::store::Status;

/// The store setting that holds where each published image lives.
pub const SETTING: &str = "images";

/// Where a published image lives.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "kebab-case")]
pub enum Home {
    /// The local Docker of the machine that builds it, loaded into the cluster's nodes.
    #[default]
    Local,
    /// A registry repository (`docker.io/acme/kiln`), pulled with `integration`'s credentials
    /// (by default the registry integration set up for it, or none: public).
    Registry {
        repository: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        integration: Option<String>,
    },
}

/// The environment variable a pipeline reads an image's home from: `KILN_IMAGE_KILN`.
pub fn env_name(name: &str) -> String {
    let up: String = name.chars().map(|c| if c.is_ascii_alphanumeric() { c.to_ascii_uppercase() } else { '_' }).collect();
    format!("KILN_IMAGE_{up}")
}

/// A name an image is published under: lower case, digits, `.`, `_`, `-`.
pub fn check_name(name: &str) -> Result<String, String> {
    let n = name.trim().to_ascii_lowercase();
    let ok = !n.is_empty() && n.len() <= 64 && n.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || matches!(b, b'.' | b'_' | b'-'));
    if ok { Ok(n) } else { Err(format!("`{name}` is not an image name: lower case letters, digits, `.`, `_` and `-`")) }
}

impl Cd {
    /// Every published image's home, by name (images not set are local).
    pub fn image_homes(&self) -> BTreeMap<String, Home> {
        self.store.read(|d| d.settings.get(SETTING).cloned()).and_then(|v| serde_json::from_value(v).ok()).unwrap_or_default()
    }

    /// Where `name` lives.
    pub fn image_home(&self, name: &str) -> Home {
        self.image_homes().remove(name).unwrap_or_default()
    }

    /// Sets where `name` lives (`None`: back to local).
    pub async fn set_image_home(&self, name: &str, home: Option<Home>) -> Result<(), String> {
        let name = check_name(name)?;
        if let Some(Home::Registry { repository, .. }) = &home {
            let r = crate::ci::integrations::oci::parse_reference(repository).map_err(|e| format!("{e:#}"))?;
            if r.tag.as_deref() != Some("latest") || r.digest.is_some() || repository.contains('@') || repository.rsplit('/').next().is_some_and(|last| last.contains(':')) {
                return Err(format!("`{repository}`: a repository, without a tag (kiln tags each image by its commit)"));
            }
        }
        let mut all = self.image_homes();
        match home {
            Some(h) if h != Home::Local => {
                all.insert(name, h);
            }
            _ => {
                all.remove(&name);
            }
        }
        let value = serde_json::to_value(&all).map_err(|e| e.to_string())?;
        match self.store.submit(Command::PutSetting(SETTING.into(), value)).await {
            Ok(Ok(())) => Ok(()),
            Ok(Err(e)) => Err(e),
            Err(_) => Err("the store stopped".into()),
        }
    }

    /// The job environment for the images people set: `KILN_IMAGE_<NAME>`, the repository.
    /// Images not set are local, which a pipeline takes from the variable's absence.
    pub fn image_env(&self) -> Vec<(String, String)> {
        self.image_homes()
            .into_iter()
            .filter_map(|(name, h)| match h {
                Home::Registry { repository, .. } => Some((env_name(&name), repository)),
                Home::Local => None,
            })
            .collect()
    }

    /// The artifact a published image stands for: an image in its registry, as any other; or
    /// `None` for a local one, whose readiness is its pipeline's run ([`Self::published_locally`]).
    pub(crate) fn published_spec(&self, spec: &ArtifactSpec) -> Option<ArtifactSpec> {
        let name = spec.published.as_deref()?;
        match self.image_home(name) {
            Home::Local => None,
            Home::Registry { repository, integration } => Some(ArtifactSpec {
                image: repository,
                tag: Some("${{ git.sha }}".into()),
                registry: integration,
                pull_secret: spec.pull_secret.clone(),
                platforms: spec.platforms.clone(),
                published: None,
                pipeline: None,
                workflow: None,
                artifact: None,
            }),
        }
    }

    /// A workflow's artifact at `revision` (docs/cd-targets.md §7): the newest run of `workflow` at
    /// that commit in the app's repository, and the content of the artifact it uploaded, by its
    /// SHA-256 (hex). A run still going, or none yet, waits; a run that failed, or that uploaded
    /// no such artifact, is an error that says which.
    pub(crate) async fn workflow_artifact(&self, def: &AppDef, workflow: &str, artifact: &str, revision: &str) -> Result<String> {
        let url = match &def.origin {
            Origin::Deploy { repo } => repo.clone(),
            _ => def.sources.first().map(|s| s.repo.clone()).unwrap_or_default(),
        };
        let repos = self.hub.store.repos().await?;
        let Some(repo) = repos.iter().find(|r| r.clone_url == url) else { bail!("{url} isn't one of kiln's repositories: its workflows' artifacts aren't kiln's") };
        let (repo_id, sha, wf, name) = (repo.id.clone(), revision.to_owned(), workflow.to_owned(), artifact.to_owned());
        type Found = (String, i64, String, Option<String>, Option<bool>);
        let found: Option<Found> = self
            .hub
            .store
            .call(move |c| {
                use rusqlite::OptionalExtension as _;
                Ok(c.query_row(
                    "SELECT r.id, r.number, r.status, a.object, a.finalized = 1 FROM runs r
                     LEFT JOIN ci_artifacts a ON a.run_id = r.id AND a.name = ?3
                     WHERE r.repo_id = ?1 AND r.sha = ?2
                       AND (lower(r.pipeline) = lower(?4) OR r.workflow_id = ?4 OR r.workflow_id LIKE '%/' || ?4 || '.yml' OR r.workflow_id LIKE '%/' || ?4 || '.yaml')
                     ORDER BY r.created_at DESC, r.number DESC LIMIT 1",
                    rusqlite::params![repo_id, sha, name, wf],
                    |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?)),
                )
                .optional()?)
            })
            .await?;
        let short = revision.get(..7).unwrap_or(revision);
        let Some((_, number, status, object, finalized)) = found else { bail!("waiting for the {workflow} workflow to run at {short}") };
        match Status::parse(&status) {
            Status::Succeeded => match (object, finalized) {
                (Some(o), Some(true)) => Ok(o),
                _ => bail!("the {workflow} workflow's run #{number} at {short} uploaded no artifact {artifact} (or it expired)"),
            },
            Status::Failed | Status::Cancelled | Status::TimedOut | Status::Skipped => bail!("the {workflow} workflow didn't build {short} (run #{number}, {status}): run it again, or deploy another commit"),
            _ => bail!("waiting for the {workflow} workflow at {short} (run #{number}, {status})"),
        }
    }

    /// Whether a local image is ready at `revision`: the run of its pipeline at that commit, in
    /// the app's repository, succeeded. Waiting (none yet, or still running) and failure are
    /// errors that say which.
    pub(crate) async fn published_locally(&self, def: &AppDef, pipeline: &str, revision: &str) -> Result<()> {
        let Origin::Deploy { repo: url } = &def.origin else {
            bail!("a published image needs an app from a repository's .kiln/deploy.yaml, whose pipeline builds it");
        };
        let repos = self.hub.store.repos().await?;
        let Some(repo) = repos.iter().find(|r| r.clone_url == *url) else { bail!("{url} isn't one of kiln's repositories") };
        let runs = self.hub.store.runs(Some(repo.id.clone()), 500, None).await?;
        let short = revision.get(..7).unwrap_or(revision);
        let run = runs.iter().filter(|r| r.sha == revision && r.pipeline.eq_ignore_ascii_case(pipeline)).max_by_key(|r| (r.created_at, r.number));
        match run {
            None => bail!("waiting for the {pipeline} pipeline to run at {short}"),
            Some(r) => match r.status {
                Status::Succeeded => Ok(()),
                Status::Failed | Status::Cancelled | Status::TimedOut | Status::Skipped => {
                    bail!("the {pipeline} pipeline didn't build {short} (run #{}, {}): run it again, or deploy another commit", r.number, r.status.as_str())
                }
                _ => bail!("waiting for the {pipeline} pipeline at {short} (run #{}, {})", r.number, r.status.as_str()),
            },
        }
    }
}

/// The images API's view: every image people set, by name.
pub fn json(homes: &BTreeMap<String, Home>) -> Value {
    serde_json::to_value(homes).unwrap_or(Value::Null)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_and_environment() {
        assert_eq!(env_name("kiln"), "KILN_IMAGE_KILN");
        assert_eq!(env_name("web-api.v2"), "KILN_IMAGE_WEB_API_V2");
        assert_eq!(check_name(" Kiln "), Ok("kiln".to_owned()));
        assert!(check_name("no spaces").is_err());
        let h: Home = serde_json::from_value(serde_json::json!({ "kind": "registry", "repository": "docker.io/acme/kiln" })).unwrap_or_default();
        assert_eq!(h, Home::Registry { repository: "docker.io/acme/kiln".into(), integration: None });
        assert_eq!(serde_json::from_value::<Home>(serde_json::json!({ "kind": "local" })).ok(), Some(Home::Local));
    }
}
