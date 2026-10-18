//! Releases from request to landed (docs/deploy-api.md §9), and the registrations they change
//! (§5).
//!
//! A release asked for is recorded first, then committed by the worker, which hands the commit to
//! the environment's provider and follows it until it has landed, failed or timed out. Each step
//! reads the release's state again and moves it with a compare-and-swap, so a cancel, a restart or
//! a second server can't make it move twice; a commit made before a crash is found again by its
//! trailer (`Kiln-Release`), never made twice.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;

use http::StatusCode;
use serde::Deserialize;
use serde_json::{Value, json};

use super::host::{Author, Change, CommitError};
use super::kilncd::{self, Landed};
use super::model::{App, Artifact, Concurrency, Environment, KILN_HOST, KILNCD, Kind, Origin, Registration, Release, ReleaseFile, State};
use super::{Deploy, repo, store};

/// An error and the status that says what went wrong.
pub type Fail = (StatusCode, String);

fn bad(msg: impl Into<String>) -> Fail {
    (StatusCode::BAD_REQUEST, msg.into())
}

fn missing(msg: impl Into<String>) -> Fail {
    (StatusCode::NOT_FOUND, msg.into())
}

fn conflict(msg: impl Into<String>) -> Fail {
    (StatusCode::CONFLICT, msg.into())
}

fn internal(e: anyhow::Error) -> Fail {
    (StatusCode::INTERNAL_SERVER_ERROR, format!("{e:#}"))
}

/// Apps and environments are named as Kubernetes names things: a DNS label.
fn dns_label(what: &str, s: &str) -> Result<(), Fail> {
    let ok = !s.is_empty()
        && s.len() <= 63
        && s.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
        && !s.starts_with('-')
        && !s.ends_with('-');
    if ok { Ok(()) } else { Err(bad(format!("{what} {s:?}: lowercase letters, digits and '-', at most 63, starting and ending with a letter or digit"))) }
}

fn emit_release(d: &Deploy, r: &Release) {
    d.hub.emit(&json!({ "type": "deploy_release", "release": r }));
}

async fn audit(d: &Deploy, actor: &str, action: &str, detail: String) {
    d.hub.store.audit(actor.to_owned(), action.to_owned(), detail).await;
}

/// The author of a commit made for `who`.
fn author(who: &str) -> Author {
    let name = if who.is_empty() { "kiln" } else { who };
    let local: String = name.chars().map(|c| if c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.') { c } else { '-' }).collect();
    Author { name: name.to_owned(), email: format!("{local}@kiln") }
}

// ------------------------------------------------------------------ environments

pub async fn put_environment(d: &Deploy, actor: &str, e: Environment) -> Result<Environment, Fail> {
    dns_label("environment", &e.name)?;
    if e.provider != KILNCD {
        return Err(bad(format!("{}: kiln deploys through {KILNCD} so far; Argo CD environments come with the Argo CD provider (docs/deploy-api.md §19, P2)", e.provider)));
    }
    if e.release_repo.host != KILN_HOST {
        return Err(bad(format!("{}: release repositories live on kiln's own git server so far; GitHub, GitLab and Bitbucket come with P3", e.release_repo.host)));
    }
    let branch = &e.release_repo.branch;
    if branch.is_empty() || branch.contains("..") || branch.chars().any(|c| c.is_whitespace() || matches!(c, '~' | '^' | ':' | '?' | '*' | '[' | '\\')) {
        return Err(bad(format!("{branch:?} isn't a branch name")));
    }
    if e.release_repo.root.split('/').any(|s| s == "..") {
        return Err(bad("the release repository's root can't climb out of it"));
    }
    if e.watch.timeout_secs < 30 || e.watch.settle_secs > e.watch.timeout_secs {
        return Err(bad("watch: timeout_secs at least 30, settle_secs at most timeout_secs"));
    }
    let (hosted, rr) = (d.hosted.clone(), e.release_repo.clone());
    crate::rt::blocking(move || hosted.ensure(&rr.repo, &rr.branch)).await.and_then(|r| r).map_err(|e| bad(format!("{e:#}")))?;
    store::put_environment(&d.hub.store, &e).await.map_err(internal)?;
    audit(d, actor, "deploy.environment.put", e.name.clone()).await;
    Ok(e)
}

pub async fn delete_environment(d: &Deploy, actor: &str, name: &str) -> Result<(), Fail> {
    let regs = store::registrations(&d.hub.store, None, Some(name)).await.map_err(internal)?;
    if !regs.is_empty() {
        let apps: Vec<&str> = regs.iter().map(|r| r.app.as_str()).collect();
        return Err(conflict(format!("{name} has apps registered: {}. Deregister them first", apps.join(", "))));
    }
    if !store::delete_environment(&d.hub.store, name).await.map_err(internal)? {
        return Err(missing(format!("no environment {name}")));
    }
    audit(d, actor, "deploy.environment.delete", name.to_owned()).await;
    Ok(())
}

async fn environment(d: &Deploy, name: &str) -> Result<Environment, Fail> {
    store::environment(&d.hub.store, name).await.map_err(internal)?.ok_or_else(|| missing(format!("no environment {name}")))
}

// ------------------------------------------------------------------ apps

pub async fn put_app(d: &Deploy, actor: &str, a: App) -> Result<App, Fail> {
    dns_label("app", &a.name)?;
    dns_label("project", &a.project)?;
    if let Some(ns) = &a.namespace {
        dns_label("namespace", ns)?;
    }
    match &a.package {
        super::model::Package::Helm { repo, chart, .. } if repo.is_empty() || chart.is_empty() => return Err(bad("a Helm package names its repository and its chart")),
        super::model::Package::Git { repo, .. } if repo.is_empty() => return Err(bad("a git package names its repository")),
        _ => {}
    }
    for (k, v) in match &a.package {
        super::model::Package::Helm { set, .. } | super::model::Package::Git { set, .. } => set,
    } {
        repo::check(v).map_err(|e| bad(format!("set {k}: {e}")))?;
    }
    store::put_app(&d.hub.store, &a).await.map_err(internal)?;
    audit(d, actor, "deploy.app.put", a.name.clone()).await;
    Ok(a)
}

pub async fn delete_app(d: &Deploy, actor: &str, name: &str) -> Result<(), Fail> {
    let regs = store::registrations(&d.hub.store, Some(name), None).await.map_err(internal)?;
    if !regs.is_empty() {
        let envs: Vec<&str> = regs.iter().map(|r| r.environment.as_str()).collect();
        return Err(conflict(format!("{name} is registered in {}. Deregister it first", envs.join(", "))));
    }
    if !store::delete_app(&d.hub.store, name).await.map_err(internal)? {
        return Err(missing(format!("no app {name}")));
    }
    audit(d, actor, "deploy.app.delete", name.to_owned()).await;
    Ok(())
}

async fn app(d: &Deploy, name: &str) -> Result<App, Fail> {
    store::app(&d.hub.store, name).await.map_err(internal)?.ok_or_else(|| missing(format!("no app {name}")))
}

// ------------------------------------------------------------------ registrations

/// The trailer kiln's registering commits carry, so no scan takes them for someone's edit.
const REGISTRATION_TRAILER: &str = "Kiln-Registration";

/// Commits `changes` on the environment's release repository, made on its head, again on a newer
/// head while someone else's push races it.
async fn commit_files(d: &Deploy, env: &Environment, changes: Vec<Change>, message: String, who: &str) -> Result<String, String> {
    let (hosted, rr, who) = (d.hosted.clone(), env.release_repo.clone(), author(who));
    crate::rt::blocking(move || -> Result<String, String> {
        for _ in 0..5 {
            let head = hosted.head(&rr.repo, &rr.branch).map_err(|e| format!("{e:#}"))?;
            match hosted.commit(&rr.repo, &rr.branch, head.as_deref(), &changes, &message, &who) {
                Ok(c) => return Ok(c.sha().to_owned()),
                Err(CommitError::Moved) => continue,
                Err(e) => return Err(e.to_string()),
            }
        }
        Err("the release repository kept moving: five tries".to_owned())
    })
    .await
    .map_err(|e| format!("{e:#}"))?
}

pub async fn register(d: &Deploy, actor: &str, app_name: &str, env_name: &str) -> Result<Registration, Fail> {
    let (a, env) = (app(d, app_name).await?, environment(d, env_name).await?);
    if a.archived {
        return Err(conflict(format!("{app_name} is archived")));
    }
    if store::registration(&d.hub.store, app_name, env_name).await.map_err(internal)?.is_some() {
        return Err(conflict(format!("{app_name} is registered in {env_name} already")));
    }
    let rr = &env.release_repo;
    let values = repo::values_file(rr, &a, env_name);
    let (hosted, repo_name, branch, values_path) = (d.hosted.clone(), rr.repo.clone(), rr.branch.clone(), values.clone());
    let has_values = crate::rt::blocking(move || -> anyhow::Result<bool> {
        let Some(head) = hosted.head(&repo_name, &branch)? else { return Ok(false) };
        Ok(hosted.read(&repo_name, &head, &values_path)?.is_some())
    })
    .await
    .and_then(|r| r)
    .map_err(internal)?;
    let mut changes = vec![
        Change { path: repo::app_file(rr, &a), content: Some(repo::app_yaml(&a).map_err(internal)?) },
        Change { path: repo::release_file(rr, &a, env_name), content: Some(repo::encode(&repo::registered_json(&a, env_name)).map_err(internal)?) },
    ];
    if !has_values {
        changes.push(Change { path: values, content: Some(format!("# {app_name}'s values in {env_name}: a config release changes them (docs/deploy-api.md §9).\n{{}}\n").into_bytes()) });
    }
    let message = format!("register {app_name} in {env_name}\n\n{REGISTRATION_TRAILER}: {app_name}/{env_name}\n{}: {actor}\n", repo::REQUESTED_BY_TRAILER);
    commit_files(d, &env, changes, message, actor).await.map_err(|e| (StatusCode::BAD_GATEWAY, e))?;
    let reg = Registration { app: app_name.to_owned(), environment: env_name.to_owned(), enabled: false, paused: false, running: None, created_at: crate::util::now_unix() };
    store::put_registration(&d.hub.store, &reg).await.map_err(internal)?;
    audit(d, actor, "deploy.register", format!("{app_name} in {env_name}")).await;
    d.hub.emit(&json!({ "type": "deploy_registration", "registration": reg }));
    Ok(reg)
}

pub async fn deregister(d: &Deploy, actor: &str, app_name: &str, env_name: &str, keep: bool) -> Result<(), Fail> {
    let (a, env) = (app(d, app_name).await?, environment(d, env_name).await?);
    let reg = registration(d, app_name, env_name).await?;
    if let Some(r) = store::in_flight(&d.hub.store, app_name, env_name).await.map_err(internal)? {
        return Err(conflict(format!("release {} is in flight for {app_name} in {env_name}: wait for it, or cancel it", r.id)));
    }
    if reg.running.is_some() || reg.enabled {
        let cd = crate::cd::get().ok_or_else(|| (StatusCode::SERVICE_UNAVAILABLE, "KilnCD isn't running: it removes what the app runs".to_owned()))?;
        kilncd::remove(&d.hub, &cd, &format!("deploy for {actor}"), &kilncd::app_id(&a, env_name), keep).await.map_err(|e| (StatusCode::BAD_GATEWAY, e))?;
    }
    let rr = &env.release_repo;
    let changes = vec![Change { path: repo::release_file(rr, &a, env_name), content: None }, Change { path: repo::values_file(rr, &a, env_name), content: None }];
    let message = format!("deregister {app_name} from {env_name}\n\n{REGISTRATION_TRAILER}: {app_name}/{env_name}\n{}: {actor}\n", repo::REQUESTED_BY_TRAILER);
    commit_files(d, &env, changes, message, actor).await.map_err(|e| (StatusCode::BAD_GATEWAY, e))?;
    store::delete_registration(&d.hub.store, app_name, env_name).await.map_err(internal)?;
    audit(d, actor, "deploy.deregister", format!("{app_name} from {env_name}{}", if keep { " (objects kept)" } else { "" })).await;
    d.hub.emit(&json!({ "type": "deploy_registration", "removed": { "app": app_name, "environment": env_name } }));
    Ok(())
}

async fn registration(d: &Deploy, app_name: &str, env_name: &str) -> Result<Registration, Fail> {
    store::registration(&d.hub.store, app_name, env_name).await.map_err(internal)?.ok_or_else(|| missing(format!("{app_name} isn't registered in {env_name}")))
}

/// Pauses (or resumes) the provider's automation for a registration, git left as it is: hand edits
/// survive until the next release, which resumes it.
pub async fn pause(d: &Deploy, actor: &str, app_name: &str, env_name: &str, paused: bool) -> Result<Registration, Fail> {
    let a = app(d, app_name).await?;
    let mut reg = registration(d, app_name, env_name).await?;
    if let Some(cd) = crate::cd::get() {
        kilncd::automation(&d.hub, &cd, &format!("deploy for {actor}"), &kilncd::app_id(&a, env_name), !paused).await.map_err(|e| (StatusCode::BAD_GATEWAY, e))?;
    }
    reg.paused = paused;
    store::put_registration(&d.hub.store, &reg).await.map_err(internal)?;
    audit(d, actor, if paused { "deploy.pause" } else { "deploy.resume" }, format!("{app_name} in {env_name}")).await;
    d.hub.emit(&json!({ "type": "deploy_registration", "registration": reg }));
    Ok(reg)
}

// ------------------------------------------------------------------ releases

/// What a release request asks for: exactly one of a version, values, a rollback or `enabled`.
#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Ask {
    pub app: String,
    pub environment: String,
    #[serde(default)]
    pub version: Option<String>,
    #[serde(default)]
    pub artifacts: BTreeMap<String, Artifact>,
    #[serde(default)]
    pub values: Option<String>,
    #[serde(default)]
    pub rollback: Option<RollbackAsk>,
    #[serde(default)]
    pub enabled: Option<bool>,
    /// Disabling: keep what the app runs, rather than remove it.
    #[serde(default)]
    pub keep: bool,
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RollbackAsk {
    /// The release to go back to; by default the newest that succeeded before the one running.
    #[serde(default)]
    pub to: Option<i64>,
}

/// The releases a registration can roll back to: the last 10 that succeeded, the one running and
/// any rolled back from left out (as txst's rule).
pub async fn candidates(d: &Deploy, app_name: &str, env_name: &str) -> Result<Vec<Release>, Fail> {
    let reg = registration(d, app_name, env_name).await?;
    let all = store::releases(&d.hub.store, store::ReleaseQuery { app: Some(app_name.into()), environment: Some(env_name.into()), state: None, limit: 500 }).await.map_err(internal)?;
    Ok(all
        .into_iter()
        .filter(|r| matches!(r.state, State::Succeeded | State::Superseded) && Some(r.id) != reg.running && r.commit.is_some() && matches!(r.kind, Kind::Deploy | Kind::Rollback | Kind::Config))
        .take(10)
        .collect())
}

pub async fn release(d: &Deploy, actor: &str, ask: Ask) -> Result<Release, Fail> {
    let a = app(d, &ask.app).await?;
    let env = environment(d, &ask.environment).await?;
    let _reg = registration(d, &ask.app, &ask.environment).await?;
    if a.archived {
        return Err(conflict(format!("{} is archived: no new releases", a.name)));
    }
    let asked = [ask.version.is_some(), ask.values.is_some(), ask.rollback.is_some(), ask.enabled.is_some()].iter().filter(|b| **b).count();
    if asked != 1 {
        return Err(bad("ask for exactly one of: a version, values, a rollback, or enabled"));
    }
    let now = crate::util::now_unix();
    let mut r = Release {
        id: 0,
        app: a.name.clone(),
        environment: env.name.clone(),
        kind: Kind::Deploy,
        state: State::Committing,
        origin: Origin::Api,
        version: None,
        artifacts: BTreeMap::new(),
        values: None,
        commit: None,
        requested_by: actor.to_owned(),
        message: None,
        rollback_of: None,
        rolled_back_by: None,
        keep: ask.keep,
        provider: Value::Null,
        created_at: now,
        committed_at: None,
        ended_at: None,
    };
    if let Some(v) = ask.version {
        if v.trim().is_empty() {
            return Err(bad("an empty version"));
        }
        r.kind = Kind::Deploy;
        r.version = Some(v.trim().to_owned());
        r.artifacts = ask.artifacts;
    } else if let Some(values) = ask.values {
        serde_yaml::from_str::<serde_yaml::Value>(&values).map_err(|e| bad(format!("the values aren't YAML: {e}")))?;
        r.kind = Kind::Config;
        r.values = Some(values);
    } else if let Some(rb) = ask.rollback {
        let options = candidates(d, &a.name, &env.name).await?;
        let to = match rb.to {
            Some(id) => options.iter().find(|c| c.id == id).cloned().ok_or_else(|| conflict(format!("release {id} isn't one {} can roll back to in {}: see its candidates", a.name, env.name)))?,
            None => options.first().cloned().ok_or_else(|| conflict(format!("{} has no earlier release in {} to roll back to", a.name, env.name)))?,
        };
        let commit = to.commit.clone().unwrap_or_default();
        let (hosted, rr) = (d.hosted.clone(), env.release_repo.clone());
        let (release_path, values_path) = (repo::release_file(&rr, &a, &env.name), repo::values_file(&rr, &a, &env.name));
        let (file, values) = crate::rt::blocking(move || -> anyhow::Result<(Option<Vec<u8>>, Option<Vec<u8>>)> {
            Ok((hosted.read(&rr.repo, &commit, &release_path)?, hosted.read(&rr.repo, &commit, &values_path)?))
        })
        .await
        .and_then(|x| x)
        .map_err(internal)?;
        let file = file.map(|b| repo::decode(&b)).transpose().map_err(internal)?.ok_or_else(|| conflict(format!("release {}'s commit holds no release.json", to.id)))?;
        r.kind = Kind::Rollback;
        r.version = file.version;
        r.artifacts = file.artifacts;
        r.values = values.map(|v| String::from_utf8_lossy(&v).into_owned());
        r.rollback_of = Some(to.id);
    } else if let Some(enabled) = ask.enabled {
        r.kind = if enabled { Kind::Enable } else { Kind::Disable };
    }
    if let Some(other) = store::in_flight(&d.hub.store, &a.name, &env.name).await.map_err(internal)? {
        match env.concurrency {
            Concurrency::Reject => return Err(conflict(format!("release {} is in flight for {} in {}: wait for it, or cancel it", other.id, a.name, env.name))),
            Concurrency::Queue => {
                r.state = State::Waiting;
                r.message = Some(format!("waits for release {}", other.id));
            }
        }
    }
    let r = store::insert_release(&d.hub.store, &r).await.map_err(internal)?;
    store::schedule(&d.hub.store, r.id, now).await.map_err(internal)?;
    d.wake.notify();
    audit(d, actor, "deploy.release", format!("{} {} in {}: release {}", r.kind.as_str(), r.version.as_deref().unwrap_or(""), r.environment, r.id)).await;
    emit_release(d, &r);
    Ok(r)
}

/// Stops a release that is waiting, or being delivered. A delivering one is already in git: the
/// environment runs what git says until a rollback says otherwise.
pub async fn cancel(d: &Deploy, actor: &str, id: i64) -> Result<Release, Fail> {
    let mut r = store::release(&d.hub.store, id).await.map_err(internal)?.ok_or_else(|| missing(format!("no release {id}")))?;
    let was = r.state;
    r.message = Some(match was {
        State::Waiting => format!("cancelled by {actor}"),
        State::Delivering => format!("cancelled by {actor}; release {id}'s commit is in git, so the environment still moves to it: roll back to undo it"),
        State::Committing => return Err(conflict(format!("release {id} is being committed right now: cancel it in a moment, or roll back"))),
        s => return Err(conflict(format!("release {id} is {}: nothing to cancel", s.as_str()))),
    });
    r.state = State::Cancelled;
    r.ended_at = Some(crate::util::now_unix());
    if !store::update_release(&d.hub.store, &r, was).await.map_err(internal)? {
        return Err(conflict(format!("release {id} moved meanwhile: look again")));
    }
    store::done(&d.hub.store, id).await.map_err(internal)?;
    d.wake.notify();
    audit(d, actor, "deploy.cancel", format!("release {id}")).await;
    emit_release(d, &r);
    Ok(r)
}

// ------------------------------------------------------------------ the worker

/// How long a server holds a release it follows before another may take it.
const LEASE_SECS: i64 = 60;
/// How often release repositories are looked at for commits kiln didn't make.
const SCAN_SECS: i64 = 30;

/// Follows every release due, until the server shuts down.
pub async fn work(d: &Arc<Deploy>) -> anyhow::Result<()> {
    let mut last_scan = 0;
    loop {
        if d.hub.shutdown.is_cancelled() {
            return Ok(());
        }
        let now = crate::util::now_unix();
        match store::claim(&d.hub.store, &d.owner, now, LEASE_SECS, 16).await {
            Ok(ids) => {
                for id in ids {
                    if let Err(e) = step(d, id).await {
                        tracing::warn!(release = id, error = %format!("{e:#}"), "deploy: a step failed; trying again shortly");
                        let _ = store::schedule(&d.hub.store, id, now + 5).await;
                    }
                }
            }
            Err(e) => tracing::warn!(error = %format!("{e:#}"), "deploy: the work queue can't be read"),
        }
        if now - last_scan >= SCAN_SECS {
            last_scan = now;
            if let Err(e) = scan_git(d).await {
                tracing::warn!(error = %format!("{e:#}"), "deploy: release repositories can't be scanned");
            }
        }
        let _ = crate::rt::timeout(Duration::from_secs(1), d.wake.notified()).await;
    }
}

/// Moves one release as far as it can go now.
async fn step(d: &Deploy, id: i64) -> anyhow::Result<()> {
    let st = &d.hub.store;
    let Some(mut r) = store::release(st, id).await? else {
        store::done(st, id).await?;
        return Ok(());
    };
    let now = crate::util::now_unix();
    let env = store::environment(st, &r.environment).await?;
    let timeout = env.as_ref().map_or(15 * 60, |e| i64::try_from(e.watch.timeout_secs).unwrap_or(900));
    match r.state {
        State::Waiting => match store::in_flight(st, &r.app, &r.environment).await? {
            Some(o) if o.id != r.id => store::schedule(st, id, now + 2).await?,
            _ => {
                r.state = State::Committing;
                r.message = None;
                if store::update_release(st, &r, State::Waiting).await? {
                    emit_release(d, &r);
                }
                store::schedule(st, id, now).await?;
            }
        },
        State::Committing => match commit_release(d, &r).await {
            Ok(sha) => {
                r.commit = Some(sha);
                r.committed_at = Some(now);
                r.state = State::Delivering;
                r.message = None;
                if !store::update_release(st, &r, State::Committing).await? {
                    return Ok(());
                }
                emit_release(d, &r);
                if let Err(e) = deliver(d, &r).await {
                    r.message = Some(e);
                    store::update_release(st, &r, State::Delivering).await?;
                }
                store::schedule(st, id, now + 2).await?;
            }
            Err(e) if now - r.created_at > timeout => finish(d, r, State::Failed, Some(format!("not committed: {e}"))).await?,
            Err(e) => {
                r.message = Some(format!("committing: {e}"));
                store::update_release(st, &r, State::Committing).await?;
                store::schedule(st, id, now + 5).await?;
            }
        },
        State::Delivering => {
            let Some(commit) = r.commit.clone() else { return finish(d, r, State::Failed, Some("delivering, but no commit was recorded".into())).await };
            let committed_at = r.committed_at.unwrap_or(r.created_at);
            let settle = env.as_ref().map_or(30, |e| i64::try_from(e.watch.settle_secs).unwrap_or(30));
            match follow(d, &r, &commit).await {
                Ok((Landed::Yes, mut view)) => {
                    let since = r.provider.get("landed_at").and_then(Value::as_i64).unwrap_or(now);
                    if now - since >= settle {
                        r.provider = view;
                        return finish(d, r, State::Succeeded, None).await;
                    }
                    if let Some(m) = view.as_object_mut() {
                        m.insert("landed_at".into(), json!(since));
                    }
                    let removed = view.get("removed").and_then(Value::as_bool) == Some(true);
                    r.provider = view;
                    r.message = Some(if removed { format!("removed; settling for {settle}s") } else { format!("synced and healthy; settling for {settle}s") });
                    store::update_release(st, &r, State::Delivering).await?;
                    store::schedule(st, id, now + settle.clamp(1, 5)).await?;
                }
                Ok((Landed::Failed(why), view)) => {
                    r.provider = view;
                    finish(d, r, State::Failed, Some(why)).await?;
                }
                Ok((Landed::No, view)) if now - committed_at > timeout => {
                    r.provider = view;
                    finish(d, r, State::Failed, Some(format!("it didn't land within {}s (watch.timeout_secs)", timeout))).await?;
                }
                Ok((Landed::No, view)) => {
                    r.provider = view;
                    r.message = None;
                    store::update_release(st, &r, State::Delivering).await?;
                    store::schedule(st, id, now + 3).await?;
                }
                Err(e) if now - committed_at > timeout => finish(d, r, State::Failed, Some(e)).await?,
                Err(e) => {
                    r.message = Some(e);
                    store::update_release(st, &r, State::Delivering).await?;
                    store::schedule(st, id, now + 5).await?;
                }
            }
        }
        _ => store::done(st, id).await?,
    }
    Ok(())
}

/// Commits a release to its environment's release repository; the commit's id. A commit made
/// before a crash is found by its trailer, not made again.
async fn commit_release(d: &Deploy, r: &Release) -> Result<String, String> {
    let a = store::app(&d.hub.store, &r.app).await.map_err(|e| format!("{e:#}"))?.ok_or_else(|| format!("no app {}", r.app))?;
    let env = store::environment(&d.hub.store, &r.environment).await.map_err(|e| format!("{e:#}"))?.ok_or_else(|| format!("no environment {}", r.environment))?;
    let rr = env.release_repo.clone();
    let (hosted, id) = (d.hosted.clone(), r.id);
    let found = {
        let rr = rr.clone();
        crate::rt::blocking(move || hosted.find_trailer(&rr.repo, &rr.branch, repo::RELEASE_TRAILER, &id.to_string())).await.and_then(|x| x).map_err(|e| format!("{e:#}"))?
    };
    if let Some(sha) = found {
        return Ok(sha);
    }
    let (hosted, release_path, values_path) = (d.hosted.clone(), repo::release_file(&rr, &a, &env.name), repo::values_file(&rr, &a, &env.name));
    let prior = {
        let (hosted, rr, release_path) = (hosted.clone(), rr.clone(), release_path.clone());
        crate::rt::blocking(move || -> anyhow::Result<Option<ReleaseFile>> {
            let Some(head) = hosted.head(&rr.repo, &rr.branch)? else { return Ok(None) };
            hosted.read(&rr.repo, &head, &release_path)?.map(|b| repo::decode(&b)).transpose()
        })
        .await
        .and_then(|x| x)
        .map_err(|e| format!("{e:#}"))?
    };
    let enabled = match r.kind {
        Kind::Disable => false,
        Kind::Config => prior.as_ref().is_some_and(|p| p.enabled),
        Kind::Deploy | Kind::Rollback | Kind::Enable => true,
    };
    let file = repo::release_json(&a, &env.name, r, prior.as_ref(), enabled);
    let mut changes = vec![
        Change { path: repo::app_file(&rr, &a), content: Some(repo::app_yaml(&a).map_err(|e| format!("{e:#}"))?) },
        Change { path: release_path, content: Some(repo::encode(&file).map_err(|e| format!("{e:#}"))?) },
    ];
    if let Some(v) = &r.values {
        changes.push(Change { path: values_path, content: Some(v.clone().into_bytes()) });
    }
    commit_files(d, &env, changes, repo::message(&a, &env.name, r), &r.requested_by).await
}

/// What a release's commit holds for its registration.
async fn release_file_at(d: &Deploy, a: &App, env: &Environment, commit: &str) -> Result<ReleaseFile, String> {
    let (hosted, rr, path, commit) = (d.hosted.clone(), env.release_repo.clone(), repo::release_file(&env.release_repo, a, &env.name), commit.to_owned());
    let bytes = crate::rt::blocking(move || hosted.read(&rr.repo, &commit, &path)).await.and_then(|x| x).map_err(|e| format!("{e:#}"))?;
    let bytes = bytes.ok_or("the commit holds no release.json for the registration")?;
    repo::decode(&bytes).map_err(|e| format!("release.json doesn't read: {e:#}"))
}

/// Hands a release's commit to KilnCD, the environment's provider.
async fn deliver(d: &Deploy, r: &Release) -> Result<(), String> {
    let commit = r.commit.clone().ok_or("no commit to deliver")?;
    let a = store::app(&d.hub.store, &r.app).await.map_err(|e| format!("{e:#}"))?.ok_or_else(|| format!("no app {}", r.app))?;
    let env = store::environment(&d.hub.store, &r.environment).await.map_err(|e| format!("{e:#}"))?.ok_or_else(|| format!("no environment {}", r.environment))?;
    let mut reg = store::registration(&d.hub.store, &r.app, &r.environment).await.map_err(|e| format!("{e:#}"))?.ok_or("the app isn't registered there any more")?;
    let cd = crate::cd::get().ok_or("KilnCD isn't running")?;
    let file = release_file_at(d, &a, &env, &commit).await?;
    let actor = format!("deploy for {}", r.requested_by);
    let id = kilncd::app_id(&a, &env.name);
    if !file.enabled {
        return kilncd::remove(&d.hub, &cd, &actor, &id, r.keep).await;
    }
    if file.version.is_none() {
        // Registered and enabled, but never given a version: nothing runs yet.
        return Ok(());
    }
    // A release resumes the provider's automation (docs/deploy-api.md §5).
    if reg.paused {
        reg.paused = false;
        store::put_registration(&d.hub.store, &reg).await.map_err(|e| format!("{e:#}"))?;
    }
    let url = d.hosted.url(&env.release_repo.repo).map_err(|e| format!("{e:#}"))?;
    let def = kilncd::definition(&a, &env, &reg, &file, &commit, &url)?;
    kilncd::deliver(&d.hub, &cd, &actor, &def).await
}

/// Where a delivering release stands, delivered again when the provider lost it (a restart
/// between the commit and the delivery).
async fn follow(d: &Deploy, r: &Release, commit: &str) -> Result<(Landed, Value), String> {
    let a = store::app(&d.hub.store, &r.app).await.map_err(|e| format!("{e:#}"))?.ok_or_else(|| format!("no app {}", r.app))?;
    let env = store::environment(&d.hub.store, &r.environment).await.map_err(|e| format!("{e:#}"))?.ok_or_else(|| format!("no environment {}", r.environment))?;
    let cd = crate::cd::get().ok_or("KilnCD isn't running")?;
    let file = release_file_at(d, &a, &env, commit).await?;
    let id = kilncd::app_id(&a, &env.name);
    let present = cd.apps.read().get(&id).map(|e| e.def.sources.iter().any(|s| s.as_ref.as_deref() == Some("release") && s.revision == commit));
    if !file.enabled {
        return Ok((if cd.apps.read().contains_key(&id) { Landed::No } else { Landed::Yes }, json!({ "app": id, "removed": true })));
    }
    if file.version.is_none() {
        return Ok((Landed::Yes, json!({ "app": id, "message": "registered and enabled; no version deployed yet" })));
    }
    if present != Some(true) {
        deliver(d, r).await?;
    }
    let since_ms = u64::try_from(r.committed_at.unwrap_or(r.created_at)).unwrap_or(0).saturating_mul(1000);
    Ok(kilncd::landed(&cd, &id, commit, since_ms))
}

/// Ends a release: its state, its registration's, and the release it replaced.
async fn finish(d: &Deploy, mut r: Release, state: State, message: Option<String>) -> anyhow::Result<()> {
    let st = &d.hub.store;
    let was = r.state;
    r.state = state;
    r.message = message;
    r.ended_at = Some(crate::util::now_unix());
    if !store::update_release(st, &r, was).await? {
        return Ok(());
    }
    store::done(st, r.id).await?;
    if state == State::Succeeded
        && let Some(mut reg) = store::registration(st, &r.app, &r.environment).await?
    {
        let before = reg.running.replace(r.id);
        match r.kind {
            Kind::Disable => reg.enabled = false,
            Kind::Deploy | Kind::Rollback | Kind::Enable => reg.enabled = true,
            Kind::Config => {}
        }
        store::put_registration(st, &reg).await?;
        if let Some(prev) = before.filter(|p| *p != r.id)
            && let Some(mut p) = store::release(st, prev).await?
            && p.state == State::Succeeded
        {
            if r.kind == Kind::Rollback {
                p.state = State::RolledBack;
                p.rolled_back_by = Some(r.id);
            } else {
                p.state = State::Superseded;
            }
            if store::update_release(st, &p, State::Succeeded).await? {
                emit_release(d, &p);
            }
        }
    }
    audit(d, &format!("deploy for {}", r.requested_by), "deploy.release.end", format!("release {} {}{}", r.id, state.as_str(), r.message.as_deref().map(|m| format!(": {m}")).unwrap_or_default())).await;
    emit_release(d, &r);
    // A release waiting behind this one may go now.
    d.wake.notify();
    Ok(())
}

/// Commits on release repositories that kiln didn't make become releases (origin `git`), so the
/// records never disagree with git (docs/deploy-api.md §6).
async fn scan_git(d: &Deploy) -> anyhow::Result<()> {
    let st = &d.hub.store;
    for env in store::environments(st).await? {
        let rr = env.release_repo.clone();
        let key = format!("deploy/seen/{}/{}", rr.repo, rr.branch);
        let seen = st.kv_get(key.clone()).await?;
        let (hosted, rr2, since) = (d.hosted.clone(), rr.clone(), seen.clone());
        let (head, commits) = crate::rt::blocking(move || -> anyhow::Result<(Option<String>, Vec<super::host::LoggedCommit>)> {
            let head = hosted.head(&rr2.repo, &rr2.branch)?;
            // The first look only notes where the branch is: history before it is kiln's own.
            let commits = match &since {
                Some(s) => hosted.commits_since(&rr2.repo, &rr2.branch, Some(s))?,
                None => Vec::new(),
            };
            Ok((head, commits))
        })
        .await
        .and_then(|x| x)?;
        let Some(head) = head else { continue };
        if seen.as_deref() == Some(head.as_str()) {
            continue;
        }
        let regs = store::registrations(st, None, Some(&env.name)).await?;
        for c in commits.iter().filter(|c| c.trailer(repo::RELEASE_TRAILER).is_none() && c.trailer(REGISTRATION_TRAILER).is_none()) {
            for reg in &regs {
                let Some(a) = store::app(st, &reg.app).await? else { continue };
                let dir = format!("{}/", repo::registration_dir(&rr, &a, &env.name));
                let app_file = repo::app_file(&rr, &a);
                if !c.paths.iter().any(|p| p.starts_with(&dir) || *p == app_file) {
                    continue;
                }
                if let Some(other) = store::in_flight(st, &reg.app, &env.name).await? {
                    tracing::warn!(app = %reg.app, environment = %env.name, commit = %c.sha, release = other.id, "deploy: a commit kiln didn't make, while a release is in flight: the next release includes it");
                    continue;
                }
                let now = crate::util::now_unix();
                let file = release_file_at(d, &a, &env, &c.sha).await.ok();
                let r = Release {
                    id: 0,
                    app: reg.app.clone(),
                    environment: env.name.clone(),
                    kind: if file.as_ref().is_some_and(|f| f.version.is_some()) && c.paths.iter().any(|p| p.ends_with("/release.json")) { Kind::Deploy } else { Kind::Config },
                    state: State::Delivering,
                    origin: Origin::Git,
                    version: file.as_ref().and_then(|f| f.version.clone()),
                    artifacts: file.map(|f| f.artifacts).unwrap_or_default(),
                    values: None,
                    commit: Some(c.sha.clone()),
                    requested_by: "git".to_owned(),
                    message: Some(format!("a commit kiln didn't make: {}", c.message.lines().next().unwrap_or(""))),
                    rollback_of: None,
                    rolled_back_by: None,
                    keep: false,
                    provider: Value::Null,
                    created_at: now,
                    committed_at: Some(now),
                    ended_at: None,
                };
                let r = store::insert_release(st, &r).await?;
                if let Err(e) = deliver(d, &r).await {
                    tracing::warn!(release = r.id, error = %e, "deploy: delivering a commit kiln didn't make");
                }
                store::schedule(st, r.id, now + 2).await?;
                emit_release(d, &r);
            }
        }
        st.kv_set(key, head).await?;
    }
    Ok(())
}
