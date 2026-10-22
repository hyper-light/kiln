//! An environment on Argo CD through its ApplicationSet (docs/deploy-api.md §8.3): what kiln keeps
//! in place there (the release repository registered with a token of kiln's that reads only it,
//! the environment's AppProject, the ApplicationSet itself), and a release delivered and followed
//! through it. The ApplicationSet makes and updates each registration's Application from its
//! `release.json`; kiln never writes one, and deletes one only once the ApplicationSet no longer
//! makes it.

use std::collections::BTreeMap;
use std::time::Duration;

use serde_json::{Value, json};

use super::argocd::{self, Allowed, Argo};
use super::model::{App, Environment, Registration, Release, ReleaseFile};
use super::provider::{Landed, Undelivered};
use super::{Deploy, repo, store};

/// The Argo CD an environment runs on through an ApplicationSet, by integration.
pub fn integration_of(env: &Environment) -> Option<&str> {
    env.provider.strip_prefix("argocd/").filter(|_| !env.argocd.applications)
}

/// How long a reader token of kiln's lasts, and how long before it ends kiln replaces it.
const TOKEN_DAYS: i64 = 90;
const TOKEN_MARGIN_SECS: i64 = 30 * 86_400;

/// Where an Argo CD reads an environment's release repository: kiln's git server, at the address
/// the instance reaches kiln at (its integration's `kiln_url`, or kiln's public URL); or GitHub.
pub async fn release_url(d: &Deploy, argo: &Argo, env: &Environment) -> String {
    if matches!(env.release_repo.host.as_str(), super::model::GITHUB_HOST | super::model::GITLAB_HOST)
        && let Some(url) = super::remote::release_url(d, &env.release_repo).await
    {
        return url;
    }
    argocd::release_repo_url(argo.kiln_url.as_deref().unwrap_or(&d.hub.cfg.server.public_url), &env.release_repo.repo)
}

/// Puts in place what an environment's ApplicationSet needs, then the ApplicationSet, read again at
/// once: the release repository registered for reading, and the environment's AppProject.
pub async fn ensure(d: &Deploy, argo: &Argo, env: &Environment) -> Result<(), Undelivered> {
    put(d, argo, env, false).await
}

/// [`ensure`], its Applications made in shadow: they compare, and never sync (§8.4).
pub async fn shadow(d: &Deploy, argo: &Argo, env: &Environment) -> Result<(), Undelivered> {
    put(d, argo, env, true).await
}

async fn put(d: &Deploy, argo: &Argo, env: &Environment, shadow: bool) -> Result<(), Undelivered> {
    let url = release_url(d, argo, env).await;
    register_repo(d, argo, env, &url).await?;
    argo.put_project(&argocd::app_project(&env.name, &allowed(d, env, &url).await?)).await?;
    argo.put_application_set(&argocd::application_set(&argo.namespace, env, &url, shadow)).await
}

/// The release repository registered with Argo CD, as a reader with a token of kiln's that reads
/// that repository only: made again a month before it ends, or when Argo CD can't read with the
/// one it has; the one it replaces revoked.
async fn register_repo(d: &Deploy, argo: &Argo, env: &Environment, url: &str) -> Result<(), String> {
    // GitHub's: read with an installation token of kiln's GitHub App.
    if matches!(env.release_repo.host.as_str(), super::model::GITHUB_HOST | super::model::GITLAB_HOST) {
        super::flow::first_commit(d, env).await?;
        return super::remote::argo_reader(d, argo, env, url).await;
    }
    let repo_name = &env.release_repo.repo;
    let note = format!("Argo CD {} reads {repo_name}", argo.integration);
    let live = crate::authz::tokens::readers(&d.hub, &note, TOKEN_MARGIN_SECS).await.map_err(|e| format!("{e:#}"))?;
    let reads = argo.repository(url).await?.is_some_and(|r| r.pointer("/connectionState/status").and_then(Value::as_str) != Some("Failed"));
    if !live.is_empty() && reads {
        return Ok(());
    }
    super::flow::first_commit(d, env).await?;
    let (id, token) = crate::authz::tokens::mint_reader(&d.hub, repo_name, Some(TOKEN_DAYS), &note).await.map_err(|e| format!("{e:#}"))?;
    if let Err(why) = argo.put_repository(url, &argocd::release_repo_name(repo_name), "kiln", &token).await {
        let _ = crate::authz::tokens::revoke_reader(&d.hub, &id).await;
        return Err(format!(
            "Argo CD can't read the release repository at {url}: {why}. It reaches kiln at its integration's kiln_url, or kiln's public URL ({})",
            d.hub.cfg.server.public_url
        ));
    }
    crate::authz::tokens::revoke_readers(&d.hub, &note, &id).await.map_err(|e| format!("{e:#}"))?;
    Ok(())
}

/// What an environment's AppProject allows: each registration's package repository and namespace,
/// its template's cluster-scoped kinds, and the release repository.
async fn allowed(d: &Deploy, env: &Environment, url: &str) -> Result<Allowed, String> {
    let st = &d.hub.store;
    let e = |e: anyhow::Error| format!("{e:#}");
    let mut out = Allowed::default();
    out.sources.insert(url.to_owned());
    let mut kinds: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for reg in store::registrations(st, None, Some(&env.name)).await.map_err(e)? {
        let Some(a) = store::app(st, &reg.app).await.map_err(e)? else { continue };
        let cluster_kinds = match &a.template {
            Some(t) => {
                if !kinds.contains_key(t) {
                    let found = store::template(st, t).await.map_err(e)?.map(|t| t.cluster_kinds).unwrap_or_default();
                    kinds.insert(t.clone(), found);
                }
                kinds.get(t).cloned().unwrap_or_default()
            }
            None => Vec::new(),
        };
        out.add(&a, env, url, &cluster_kinds);
    }
    Ok(out)
}

/// Whether an environment's ApplicationSet has read the release repository since it was last
/// asked to: its refresh is done. Any error it reports is said too.
async fn set_read(argo: &Argo, env: &Environment) -> Result<(bool, Option<String>), String> {
    let Some(set) = argo.application_set(&argocd::set_name(&env.name)).await? else { return Ok((false, Some("it has no ApplicationSet".into()))) };
    let refreshing = set.pointer("/metadata/annotations/argocd.argoproj.io~1application-set-refresh").is_some();
    let error = set
        .pointer("/status/conditions")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .find(|c| c.get("type").and_then(Value::as_str) == Some("ErrorOccurred") && c.get("status").and_then(Value::as_str) == Some("True"))
        .and_then(|c| c.get("message").and_then(Value::as_str))
        .map(|m| format!("its ApplicationSet, which says: {m}"));
    Ok((!refreshing, error))
}

/// Deletes a registration's Application (what it runs too, unless `keep`) once the ApplicationSet
/// has read the release repository again, which no longer makes it: deleted before, it would be
/// made again. Waits for that up to a minute.
pub async fn remove(d: &Deploy, argo: &Argo, env: &Environment, name: &str, keep: bool) -> Result<(), String> {
    ensure(d, argo, env).await.map_err(|e| e.to_string())?;
    let mut waited = Duration::ZERO;
    loop {
        if set_read(argo, env).await?.0 {
            break;
        }
        if waited >= Duration::from_secs(60) {
            return Err(format!("Argo CD's ApplicationSet {} hasn't read the release repository in a minute: try again", argocd::set_name(&env.name)));
        }
        if !d.hub.shutdown.sleep(Duration::from_secs(2)).await {
            return Err("kiln is stopping".into());
        }
        waited += Duration::from_secs(2);
    }
    argo.delete_application(name, keep).await
}

/// What a release's follow carries from one look to the next.
const REFRESHED: &str = "refreshed_for";

/// Where a release stands on Argo CD through its environment's ApplicationSet. Its Application must
/// be made from its `release.json` (the ApplicationSet is asked again until it is), compared with
/// the release repository at a commit holding its files, and synced and healthy. A disabled one
/// lands once its Application is gone.
#[allow(clippy::too_many_arguments)]
pub async fn follow(d: &Deploy, argo: &Argo, r: &Release, a: &App, env: &Environment, reg: &Registration, file: &ReleaseFile, commit: &str) -> Result<(Landed, Value), String> {
    let name = argocd::application_name(a, &env.name);
    let watched = d.argo.application(&argo.integration, &name);
    let streamed = watched.is_some();
    let current = match watched {
        Some(app) => Some(app),
        None => argo.application(&name, None).await?,
    };
    let refreshed = r.provider.get(REFRESHED).and_then(Value::as_i64);
    let waiting = |what: String| json!({ "application": name, "waiting": what, "watched": streamed, REFRESHED: refreshed });
    if !file.enabled {
        let Some(cur) = current else { return Ok((Landed::Yes, json!({ "application": name, "removed": true }))) };
        if cur.pointer("/metadata/deletionTimestamp").is_some() {
            return Ok((Landed::No, waiting("Argo CD to delete it".into())));
        }
        let (read, error) = set_read(argo, env).await?;
        if !read {
            return Ok((Landed::No, waiting(error.unwrap_or_else(|| "its ApplicationSet to read the release repository, before its Application is deleted".into()))));
        }
        argo.delete_application(&name, r.keep).await?;
        return Ok((Landed::No, waiting(format!("Argo CD to delete it{}", if r.keep { ", keeping what it runs" } else { "" }))));
    }
    let Some(version) = file.version.clone() else {
        return Ok((Landed::Yes, json!({ "application": name, "message": "registered and enabled; no version deployed yet" })));
    };
    // Not made from this release's file yet: the ApplicationSet is asked to read it again, once
    // it has done with the last ask.
    let Some(cur) = current.filter(|c| argocd::made_for(c) == Some(file.release)) else {
        let (read, error) = set_read(argo, env).await?;
        if read {
            ensure(d, argo, env).await.map_err(|e| e.to_string())?;
        }
        return Ok((Landed::No, waiting(error.unwrap_or_else(|| format!("its ApplicationSet to make {name} from release {}'s release.json", file.release)))));
    };
    // A release turns automation back on, paused before or by hand (§5), as the environment says
    // it now: the ApplicationSet leaves an Application's sync policy to kiln.
    let want = (env.sync.prune, env.sync.self_heal);
    let flag = |a: &Value, k: &str| a.get(k).and_then(Value::as_bool).unwrap_or(false);
    let has = cur.pointer("/spec/syncPolicy/automated").map(|a| (flag(a, "prune"), flag(a, "selfHeal")));
    if !reg.paused && has != Some(want) {
        argo.set_automation(&name, Some(want)).await?;
    }
    // Compared again at once, rather than at Argo CD's next look: once a release.
    let mut refreshed = refreshed;
    if refreshed != Some(r.id) {
        argo.application(&name, Some("hard")).await?;
        refreshed = Some(r.id);
    }
    let (landed, mut view) = argocd::landed(&cur, file.release, &version);
    // Its values come from the release repository: compared at a commit that holds the release's
    // files (a later one holds them too, until the next release of it).
    let landed = match (landed, argocd::release_revision(&cur)) {
        (Landed::Yes, Some(rev)) if !holds(d, a, env, &rev, commit).await? => Landed::No,
        (l, _) => l,
    };
    if let Some(m) = view.as_object_mut() {
        m.insert("watched".into(), json!(streamed));
        m.insert(REFRESHED.into(), json!(refreshed));
    }
    Ok((landed, view))
}

/// Whether the release repository at `rev` holds a registration's files as `commit` has them.
async fn holds(d: &Deploy, a: &App, env: &Environment, rev: &str, commit: &str) -> Result<bool, String> {
    if rev == commit {
        return Ok(true);
    }
    let rr = &env.release_repo;
    let at = super::git::open(d, rr).await?;
    // A commit the release repository doesn't have isn't one that holds them.
    for p in [repo::release_file(rr, a, &env.name), repo::values_file(rr, a, &env.name)] {
        match (at.read(rev, &p).await, at.read(commit, &p).await) {
            (Ok(x), Ok(y)) if x == y => {}
            _ => return Ok(false),
        }
    }
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::deploy::model::{ArgoSettings, Environment};

    #[test]
    fn an_environment_runs_through_an_application_set_unless_kiln_writes_its_applications() {
        let env = |provider: &str, applications: bool| Environment {
            name: "production".into(),
            provider: provider.into(),
            release_repo: Default::default(),
            targets: Default::default(),
            gates: Default::default(),
            sync: Default::default(),
            watch: Default::default(),
            concurrency: Default::default(),
            agents: Default::default(),
            argocd: ArgoSettings { applications },
        };
        assert_eq!(integration_of(&env("argocd/prod", false)), Some("prod"));
        assert_eq!(integration_of(&env("argocd/prod", true)), None, "kiln writes its Applications");
        assert_eq!(integration_of(&env("kilncd", false)), None);
    }
}
