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

use super::argocd;
use super::host::{Author, Change, CommitError};
use super::kilncd;
use super::provider::{Landed, Undelivered};
use super::gates::{self, Verdict};
use super::model::{App, Approval, Artifact, Concurrency, Environment, KILN_HOST, KILNCD, Kind, Origin, Registration, Release, ReleaseFile, State};
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

/// Tells every client, and whoever waits on it, that a release moved; one waiting for approval
/// says so, and who may give it (`needs`, `approvers`).
fn emit_release(d: &Deploy, r: &Release) {
    let mut e = json!({
        "type": "deploy",
        "id": r.id,
        "app": r.app,
        "environment": r.environment,
        "kind": r.kind,
        "state": r.state,
        "by": r.requested_by,
        "release": r,
    });
    if r.state == State::WaitingApproval
        && let Some(m) = e.as_object_mut()
    {
        let reviewers = r.gates.iter().find(|g| g.gate == "approvals").and_then(|g| g.evidence.get("reviewers")).cloned().unwrap_or(Value::Null);
        m.insert("needs".into(), json!("approval"));
        m.insert("approvers".into(), reviewers);
    }
    d.hub.emit(&e);
    d.changes.send(r.id);
}

/// Who asks for a change: the name the audit, the release and its commit say, and whether it is
/// an agent, which releases only where the environment lets agents (docs/cd-targets.md D6).
#[derive(Debug, Clone)]
pub struct Who {
    pub name: String,
    pub agent: bool,
}

impl Who {
    pub fn of(caller: &crate::authz::Caller) -> Self {
        Self { name: caller.name.clone(), agent: caller.kind == crate::authz::gate::Kind::Agent }
    }
}

/// Agents change what runs only where the environment lets them.
fn agents_may(who: &Who, env: &Environment, what: &str) -> Result<(), Fail> {
    if !who.agent || env.agents.deploy {
        return Ok(());
    }
    Err((
        StatusCode::FORBIDDEN,
        format!("agents may not {what} in {}: the environment doesn't let agents deploy. Ask a person to, or have them allow it (agents: {{ deploy: true }})", env.name),
    ))
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

/// The Argo CD integration an environment deploys through, if it isn't KilnCD.
fn argo_of(env: &Environment) -> Option<&str> {
    env.provider.strip_prefix("argocd/")
}

/// The provider's name, as messages say it.
fn provider_name(env: &Environment) -> &'static str {
    if argo_of(env).is_some() { "Argo CD" } else { "KilnCD" }
}

/// A commit, as messages show it.
fn short(sha: &str) -> &str {
    sha.get(..12).unwrap_or(sha)
}

/// Sets `key` on a release's provider view, which becomes an object if it wasn't one.
fn note(view: &mut Value, key: &str, v: Value) {
    if !view.is_object() {
        *view = json!({});
    }
    if let Some(m) = view.as_object_mut() {
        m.insert(key.to_owned(), v);
    }
}

pub async fn put_environment(d: &Deploy, who: &Who, e: Environment) -> Result<Environment, Fail> {
    let actor = who.name.as_str();
    dns_label("environment", &e.name)?;
    // What agents may do is a person's to say.
    if who.agent {
        let before = store::environment(&d.hub.store, &e.name).await.map_err(internal)?.map(|b| b.agents).unwrap_or_default();
        if e.agents != before {
            return Err((StatusCode::FORBIDDEN, format!("agents may not change what agents may do in {}: a person does", e.name)));
        }
    }
    match argo_of(&e) {
        Some(name) => {
            let i = crate::ci::integrations::get(&d.hub, name).await.map_err(internal)?.ok_or_else(|| bad(format!("no integration {name}: add the Argo CD instance in Settings → Integrations first")))?;
            if i.kind != argocd::KIND {
                return Err(bad(format!("{name} is a {} integration, not Argo CD", i.kind)));
            }
        }
        None if e.provider == KILNCD => {}
        None => return Err(bad(format!("{}: a provider is {KILNCD} or argocd/<integration>", e.provider))),
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
    if e.watch.timeout_secs < 30 || e.watch.settle_secs > e.watch.timeout_secs || e.watch.refused_secs > e.watch.timeout_secs {
        return Err(bad("watch: timeout_secs at least 30, settle_secs and refused_secs at most timeout_secs"));
    }
    ensure_release_repo(d, &e.release_repo).await.map_err(bad)?;
    store::put_environment(&d.hub.store, &e).await.map_err(internal)?;
    // Its gates may have changed: what waits on them looks again now.
    let held = store::releases(&d.hub.store, store::ReleaseQuery { app: None, environment: Some(e.name.clone()), state: None, limit: 1000 }).await.map_err(internal)?;
    let now = crate::util::now_unix();
    for r in held.iter().filter(|r| gates::holding(r.state)) {
        store::schedule(&d.hub.store, r.id, now).await.map_err(internal)?;
    }
    d.wake.notify();
    audit(d, actor, "deploy.environment.put", e.name.clone()).await;
    Ok(e)
}

/// An environment's release repository as one of kiln's native repositories (docs/ci.md W14): its
/// bare repository, and its row, so kiln's git server serves it to readers with a token, its review
/// rules hold, and people change it in diffs as any repository's.
pub async fn ensure_release_repo(d: &Deploy, rr: &super::model::RepoRef) -> Result<(), String> {
    if crate::ci::hosting::path_of(&d.hub, &rr.repo).is_none() {
        return Err(format!("{}: a release repository on kiln's git server is `owner/name`, of letters, digits, '-', '_' and '.'", rr.repo));
    }
    let repos = d.hub.store.repos().await.map_err(|e| format!("{e:#}"))?;
    let row = repos.iter().find(|r| r.name == rr.repo);
    if let Some(r) = row
        && r.mode != "native"
    {
        return Err(format!("{} is a repository kiln follows upstream: a release repository on kiln's git server is one of kiln's own, so name another", rr.repo));
    }
    let path = crate::ci::hosting::create(&d.hub, &rr.repo, &rr.branch, None).await.map_err(|e| format!("{e:#}"))?;
    if row.is_some() {
        return Ok(());
    }
    d.hub
        .store
        .upsert_repo(crate::store::Repo {
            id: crate::util::new_id(),
            name: rr.repo.clone(),
            clone_url: crate::ci::hosting::clone_url(&path),
            branches: Vec::new(),
            poll_seconds: d.hub.cfg.gitops.poll_seconds,
            enabled: true,
            created_at: crate::util::now_unix(),
            last_polled_at: None,
            last_error: None,
            mode: "native".into(),
            default_branch: Some(rr.branch.clone()),
            mirror: None,
        })
        .await
        .map_err(|e| format!("{e:#}"))?;
    d.hub.store.audit("kiln".into(), "repo.add".into(), format!("{} (a release repository, docs/deploy-api.md §6)", rr.repo)).await;
    d.hub.gitops_kick.notify();
    Ok(())
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

pub async fn deregister(d: &Deploy, who: &Who, app_name: &str, env_name: &str, keep: bool) -> Result<(), Fail> {
    let actor = who.name.as_str();
    let (a, env) = (app(d, app_name).await?, environment(d, env_name).await?);
    agents_may(who, &env, "deregister apps")?;
    let reg = registration(d, app_name, env_name).await?;
    if let Some(r) = store::in_flight(&d.hub.store, app_name, env_name).await.map_err(internal)? {
        return Err(conflict(format!("release {} is in flight for {app_name} in {env_name}: wait for it, or cancel it", r.id)));
    }
    if reg.running.is_some() || reg.enabled {
        match argo_of(&env) {
            Some(name) => {
                let argo = argocd::Argo::of(&d.hub, name).await.map_err(|e| (StatusCode::BAD_GATEWAY, e))?;
                argo.delete_application(&argocd::application_name(&a, env_name), keep).await.map_err(|e| (StatusCode::BAD_GATEWAY, e))?;
            }
            None => {
                let cd = crate::cd::get().ok_or_else(|| (StatusCode::SERVICE_UNAVAILABLE, "KilnCD isn't running: it removes what the app runs".to_owned()))?;
                kilncd::remove(&d.hub, &cd, &format!("deploy for {actor}"), &kilncd::app_id(&a, env_name), keep).await.map_err(|e| (StatusCode::BAD_GATEWAY, e))?;
            }
        }
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
    let env = environment(d, env_name).await?;
    let mut reg = registration(d, app_name, env_name).await?;
    match argo_of(&env) {
        Some(name) => {
            let argo = argocd::Argo::of(&d.hub, name).await.map_err(|e| (StatusCode::BAD_GATEWAY, e))?;
            let automated = (!paused).then_some((env.sync.prune, env.sync.self_heal));
            argo.set_automation(&argocd::application_name(&a, env_name), automated).await.map_err(|e| (StatusCode::BAD_GATEWAY, e))?;
        }
        None => {
            if let Some(cd) = crate::cd::get() {
                kilncd::automation(&d.hub, &cd, &format!("deploy for {actor}"), &kilncd::app_id(&a, env_name), !paused).await.map_err(|e| (StatusCode::BAD_GATEWAY, e))?;
            }
        }
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
    /// A promotion: exactly what this environment runs, its version and artifacts (§9.2).
    #[serde(default)]
    pub promote_from: Option<String>,
    /// Disabling: keep what the app runs, rather than remove it.
    #[serde(default)]
    pub keep: bool,
    /// Asked for again with the same key, it is the same release (a retry after a timeout).
    #[serde(default)]
    pub idempotency_key: Option<String>,
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

pub async fn release(d: &Deploy, who: &Who, ask: Ask) -> Result<Release, Fail> {
    let actor = who.name.as_str();
    let a = app(d, &ask.app).await?;
    let env = environment(d, &ask.environment).await?;
    let _reg = registration(d, &ask.app, &ask.environment).await?;
    agents_may(who, &env, "release apps")?;
    let key = ask.idempotency_key.as_deref().map(str::trim).filter(|k| !k.is_empty()).map(str::to_owned);
    if key.as_ref().is_some_and(|k| k.len() > 255) {
        return Err(bad("an idempotency key is at most 255 characters"));
    }
    if a.archived {
        return Err(conflict(format!("{} is archived: no new releases", a.name)));
    }
    let asked = [ask.version.is_some(), ask.values.is_some(), ask.rollback.is_some(), ask.enabled.is_some(), ask.promote_from.is_some()].iter().filter(|b| **b).count();
    if asked != 1 {
        return Err(bad("ask for exactly one of: a version, values, a rollback, enabled, or promote_from"));
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
        gates: Vec::new(),
        approvals: Vec::new(),
        promoted_from: None,
        idempotency_key: key,
        created_at: now,
        committed_at: None,
        ended_at: None,
    };
    // Asked for before with this key: that release, whatever it has become.
    if let Some(k) = &r.idempotency_key
        && let Some(same) = store::release_by_key(&d.hub.store, &a.name, &env.name, k).await.map_err(internal)?
    {
        return Ok(same);
    }
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
    } else if let Some(from) = ask.promote_from {
        if from == env.name {
            return Err(bad("a promotion comes from another environment"));
        }
        let up = registration(d, &a.name, &from).await?;
        let running = match up.running {
            Some(id) => store::release(&d.hub.store, id).await.map_err(internal)?,
            None => None,
        };
        let running = running.filter(|x| x.version.is_some()).ok_or_else(|| conflict(format!("nothing of {} runs in {from} to promote", a.name)))?;
        r.kind = Kind::Deploy;
        r.version = running.version.clone();
        r.artifacts = running.artifacts.clone();
        r.promoted_from = Some(running.id);
    }
    // The version is where the package comes from (docs/deploy-api.md §9, step 1).
    if matches!(r.kind, Kind::Deploy | Kind::Rollback) {
        let file = repo::release_json(&a, &env.name, &r, None, true);
        if let Some(why) = super::check::missing_version(&d.hub, &a.package, &file).await {
            return Err((StatusCode::UNPROCESSABLE_ENTITY, why));
        }
    }
    // The gates (§9.1): one it can't pass by waiting refuses it now; one that holds it, it waits on.
    match gates::evaluate(d, &r, &env, &a).await.map_err(internal)? {
        Verdict::Refuse { why, .. } => return Err(conflict(why)),
        Verdict::Hold { results, why, approval, .. } => {
            r.gates = results;
            r.state = if approval { State::WaitingApproval } else { State::Waiting };
            r.message = Some(why);
        }
        Verdict::Go(results) => r.gates = results,
    }
    if let Some(other) = store::in_flight(&d.hub.store, &a.name, &env.name).await.map_err(internal)? {
        match env.concurrency {
            Concurrency::Reject => return Err(conflict(format!("release {} is in flight for {} in {}: wait for it, or cancel it", other.id, a.name, env.name))),
            Concurrency::Queue if r.state == State::Committing => {
                r.state = State::Waiting;
                r.message = Some(format!("waits for release {}", other.id));
            }
            Concurrency::Queue => {}
        }
    }
    let (r, new) = store::insert_release(&d.hub.store, &r).await.map_err(internal)?;
    if !new {
        return Ok(r);
    }
    store::schedule(&d.hub.store, r.id, now).await.map_err(internal)?;
    d.wake.notify();
    audit(d, actor, "deploy.release", format!("{} {} in {}: release {}", r.kind.as_str(), r.version.as_deref().unwrap_or(""), r.environment, r.id)).await;
    emit_release(d, &r);
    Ok(r)
}

/// Stops a release that is waiting, or being delivered. A delivering one is in git already: the
/// worker reverts its commit, and hands the provider what ran before (docs/deploy-api.md §9).
pub async fn cancel(d: &Deploy, who: &Who, id: i64, again: bool) -> Result<Release, Fail> {
    let actor = who.name.as_str();
    let mut r = store::release(&d.hub.store, id).await.map_err(internal)?.ok_or_else(|| missing(format!("no release {id}")))?;
    agents_may(who, &environment(d, &r.environment).await?, "cancel releases")?;
    let was = r.state;
    match was {
        State::Waiting | State::WaitingApproval | State::Delivering => {}
        // A retried cancel (with an idempotency key) finds it cancelled already.
        State::Cancelled if again => return Ok(r),
        State::Committing => return Err(conflict(format!("release {id} is being committed right now: cancel it in a moment, or roll back"))),
        s => return Err(conflict(format!("release {id} is {}: nothing to cancel", s.as_str()))),
    }
    let now = crate::util::now_unix();
    r.message = Some(format!("cancelled by {actor}"));
    r.state = State::Cancelled;
    r.ended_at = Some(now);
    let undo = was == State::Delivering;
    if undo {
        note(&mut r.provider, "undo", json!(true));
    }
    if !store::update_release(&d.hub.store, &r, was).await.map_err(internal)? {
        return Err(conflict(format!("release {id} moved meanwhile: look again")));
    }
    if undo {
        store::schedule(&d.hub.store, id, now).await.map_err(internal)?;
    } else {
        store::done(&d.hub.store, id).await.map_err(internal)?;
    }
    d.wake.notify();
    audit(d, actor, "deploy.cancel", format!("release {id}")).await;
    emit_release(d, &r);
    Ok(r)
}

/// A person's answer to a release waiting for approval (docs/deploy-api.md §9.1): an approval
/// counts toward the environment's `approvals`, and the release goes once there are enough; a
/// rejection cancels it.
pub async fn answer(d: &Deploy, who: &Who, id: i64, approved: bool, note: Option<String>) -> Result<Release, Fail> {
    let mut r = store::release(&d.hub.store, id).await.map_err(internal)?.ok_or_else(|| missing(format!("no release {id}")))?;
    let env = environment(d, &r.environment).await?;
    if who.agent && !env.agents.approve {
        return Err((StatusCode::FORBIDDEN, format!("agents may not answer approvals in {}: a person does, unless they allow it (agents: {{ approve: true }})", env.name)));
    }
    let Some(rules) = env.gates.approvals.clone() else { return Err(conflict(format!("{} asks for no approvals", env.name))) };
    if !gates::holding(r.state) {
        return Err(conflict(format!("release {id} is {}: nothing waits for an answer", r.state.as_str())));
    }
    let me = gates::person(&who.name).to_owned();
    if !rules.reviewers.is_empty() && !rules.reviewers.contains(&me) {
        return Err((StatusCode::FORBIDDEN, format!("{me} isn't one of {}'s reviewers ({})", env.name, rules.reviewers.join(", "))));
    }
    if approved && !rules.self_approve && me == gates::person(&r.requested_by) {
        return Err((StatusCode::FORBIDDEN, format!("{me} asked for release {id}: someone else approves it")));
    }
    let now = crate::util::now_unix();
    let note = note.map(|n| n.trim().to_owned()).filter(|n| !n.is_empty());
    let was = r.state;
    r.approvals.retain(|x| gates::person(&x.by) != me);
    r.approvals.push(Approval { by: who.name.clone(), at: now, approved, note: note.clone() });
    if !approved {
        r.state = State::Cancelled;
        r.ended_at = Some(now);
        r.message = Some(format!("rejected by {}{}", who.name, note.as_deref().map(|n| format!(": {n}")).unwrap_or_default()));
    }
    if !store::update_release(&d.hub.store, &r, was).await.map_err(internal)? {
        return Err(conflict(format!("release {id} moved meanwhile: look again")));
    }
    if approved {
        store::schedule(&d.hub.store, id, now).await.map_err(internal)?;
    } else {
        store::done(&d.hub.store, id).await.map_err(internal)?;
    }
    d.wake.notify();
    audit(d, &who.name, if approved { "deploy.approve" } else { "deploy.reject" }, format!("release {id}")).await;
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
        State::Waiting | State::WaitingApproval => match store::in_flight(st, &r.app, &r.environment).await? {
            Some(o) if o.id != r.id => store::schedule(st, id, now + 2).await?,
            _ => {
                let was = r.state;
                let (Some(env), Some(a)) = (env.clone(), store::app(st, &r.app).await?) else {
                    return finish(d, r, State::Failed, Some("its app or environment is gone".into())).await;
                };
                match gates::evaluate(d, &r, &env, &a).await? {
                    Verdict::Go(results) => {
                        r.gates = results;
                        r.state = State::Committing;
                        r.message = None;
                        if store::update_release(st, &r, was).await? {
                            emit_release(d, &r);
                        }
                        store::schedule(st, id, now).await?;
                    }
                    Verdict::Hold { results, why, approval, until } => {
                        let state = if approval { State::WaitingApproval } else { State::Waiting };
                        let moved = state != r.state || r.message.as_deref() != Some(why.as_str());
                        r.gates = results;
                        r.state = state;
                        r.message = Some(why);
                        if store::update_release(st, &r, was).await? && moved {
                            emit_release(d, &r);
                        }
                        // An answer wakes it at once; otherwise it looks again when the gate says,
                        // and every five minutes at least.
                        store::schedule(st, id, until.unwrap_or(now + 300).clamp(now + 1, now + 300)).await?;
                    }
                    Verdict::Refuse { results, why } => {
                        r.gates = results;
                        finish(d, r, State::Failed, Some(why)).await?;
                    }
                }
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
                match deliver(d, &r).await {
                    Ok(()) => note(&mut r.provider, "delivered_at", json!(now)),
                    Err(e) => r.message = Some(e.to_string()),
                }
                store::update_release(st, &r, State::Delivering).await?;
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
            let delivered = r.provider.get("delivered_at").and_then(Value::as_i64);
            // When the provider first held the release: kept from one look to the next.
            let looked = follow(d, &r, &commit).await.map(|(landed, mut view)| {
                let held = !matches!(landed, Landed::Refused(_));
                if let Some(at) = delivered.or(held.then_some(now)) {
                    note(&mut view, "delivered_at", json!(at));
                }
                (landed, view)
            });
            match looked {
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
                Ok((Landed::Refused(why), mut view)) => {
                    let since = r.provider.get("refused_at").and_then(Value::as_i64).unwrap_or(now);
                    let grace = env.as_ref().map_or(60, |e| i64::try_from(e.watch.refused_secs).unwrap_or(60));
                    let name = env.as_ref().map_or("the provider", provider_name);
                    note(&mut view, "refused_at", json!(since));
                    r.provider = view;
                    if now - since >= grace {
                        let undone = give_back(d, &r, &format!("{name} refused it: {why}")).await;
                        return finish(d, r, State::Failed, Some(format!("{name} refused it: {why}; {undone}"))).await;
                    }
                    r.message = Some(format!("{name} refuses it: {why} (it fails in {}s unless that clears)", grace - (now - since)));
                    store::update_release(st, &r, State::Delivering).await?;
                    store::schedule(st, id, now + 5).await?;
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
                // The provider never held it: nothing changed there, and git goes back too.
                Err(e) if now - committed_at > timeout && delivered.is_none() => {
                    let undone = give_back(d, &r, &format!("it never reached the provider: {e}")).await;
                    finish(d, r, State::Failed, Some(format!("it never reached the provider within {timeout}s: {e}; {undone}"))).await?;
                }
                Err(e) if now - committed_at > timeout => finish(d, r, State::Failed, Some(e)).await?,
                Err(e) => {
                    r.message = Some(e);
                    store::update_release(st, &r, State::Delivering).await?;
                    store::schedule(st, id, now + 5).await?;
                }
            }
        }
        State::Cancelled if r.provider.get("undo").and_then(Value::as_bool) == Some(true) => {
            let undone = undo(d, &r, &format!("{} (release {} was delivering)", r.message.as_deref().unwrap_or("cancelled"), r.id)).await;
            note(&mut r.provider, "undo", json!(false));
            r.message = Some(format!("{}; {undone}", r.message.as_deref().unwrap_or("cancelled")));
            if store::update_release(st, &r, State::Cancelled).await? {
                emit_release(d, &r);
            }
            store::done(st, id).await?;
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

/// A file of a release's commit, for its registration (`None`: the commit has none).
async fn file_at(d: &Deploy, env: &Environment, commit: &str, path: String) -> Result<Option<String>, String> {
    let (hosted, rr, commit) = (d.hosted.clone(), env.release_repo.clone(), commit.to_owned());
    let bytes = crate::rt::blocking(move || hosted.read(&rr.repo, &commit, &path)).await.and_then(|x| x).map_err(|e| format!("{e:#}"))?;
    Ok(bytes.map(|b| String::from_utf8_lossy(&b).into_owned()))
}

/// Hands a release's commit to the environment's provider.
async fn deliver(d: &Deploy, r: &Release) -> Result<(), Undelivered> {
    let commit = r.commit.clone().ok_or("no commit to deliver")?;
    let a = store::app(&d.hub.store, &r.app).await.map_err(|e| format!("{e:#}"))?.ok_or_else(|| format!("no app {}", r.app))?;
    let env = store::environment(&d.hub.store, &r.environment).await.map_err(|e| format!("{e:#}"))?.ok_or_else(|| format!("no environment {}", r.environment))?;
    let mut reg = store::registration(&d.hub.store, &r.app, &r.environment).await.map_err(|e| format!("{e:#}"))?.ok_or("the app isn't registered there any more")?;
    let file = release_file_at(d, &a, &env, &commit).await?;
    // A release resumes the provider's automation (docs/deploy-api.md §5).
    if file.enabled && file.version.is_some() && reg.paused {
        reg.paused = false;
        store::put_registration(&d.hub.store, &reg).await.map_err(|e| format!("{e:#}"))?;
    }
    if let Some(name) = argo_of(&env) {
        let argo = argocd::Argo::of(&d.hub, name).await?;
        let app_name = argocd::application_name(&a, &env.name);
        if !file.enabled {
            return Ok(argo.delete_application(&app_name, r.keep).await?);
        }
        if file.version.is_none() {
            return Ok(());
        }
        if let super::model::Package::Helm { repo, .. } = &a.package
            && let Some(oci) = repo.strip_prefix("oci://")
        {
            argo.ensure_oci_repo(oci.trim_end_matches('/')).await?;
        }
        let values = file_at(d, &env, &commit, repo::values_file(&env.release_repo, &a, &env.name)).await?;
        let spec = argocd::application(&argo.namespace, &a, &env, &reg, &file, values.as_deref(), &commit).map_err(Undelivered::Refused)?;
        argo.put_application(&spec).await?;
        // Compared again at once, rather than at Argo CD's next look (3 minutes by default).
        argo.application(&app_name, Some("hard")).await?;
        return Ok(());
    }
    let cd = crate::cd::get().ok_or("KilnCD isn't running")?;
    let actor = format!("deploy for {}", r.requested_by);
    let id = kilncd::app_id(&a, &env.name);
    if !file.enabled {
        return Ok(kilncd::remove(&d.hub, &cd, &actor, &id, r.keep).await?);
    }
    if file.version.is_none() {
        // Registered and enabled, but never given a version: nothing runs yet.
        return Ok(());
    }
    let url = d.hosted.url(&env.release_repo.repo).map_err(|e| format!("{e:#}"))?;
    let def = kilncd::definition(&a, &env, &reg, &file, &commit, &url).map_err(Undelivered::Refused)?;
    kilncd::deliver(&d.hub, &cd, &actor, &def).await
}

/// Where a delivering release stands, delivered again when the provider lost it (a restart
/// between the commit and the delivery).
async fn follow(d: &Deploy, r: &Release, commit: &str) -> Result<(Landed, Value), String> {
    let a = store::app(&d.hub.store, &r.app).await.map_err(|e| format!("{e:#}"))?.ok_or_else(|| format!("no app {}", r.app))?;
    let env = store::environment(&d.hub.store, &r.environment).await.map_err(|e| format!("{e:#}"))?.ok_or_else(|| format!("no environment {}", r.environment))?;
    let file = release_file_at(d, &a, &env, commit).await?;
    if let Some(name) = argo_of(&env) {
        let argo = argocd::Argo::of(&d.hub, name).await?;
        let app_name = argocd::application_name(&a, &env.name);
        let current = argo.application(&app_name, None).await?;
        if !file.enabled {
            // Still there and not on its way out: the delete didn't happen, and is asked for again.
            if current.as_ref().is_some_and(|c| c.pointer("/metadata/deletionTimestamp").is_none()) {
                deliver(d, r).await.map_err(|e| e.to_string())?;
            }
            return Ok((if current.is_some() { Landed::No } else { Landed::Yes }, json!({ "application": app_name, "removed": true })));
        }
        let Some(version) = file.version.clone() else {
            return Ok((Landed::Yes, json!({ "application": app_name, "message": "registered and enabled; no version deployed yet" })));
        };
        // An Application that isn't this release's (lost to a restart, or replaced by hand) is
        // written again.
        let stamped = current.as_ref().and_then(|c| c.pointer("/metadata/annotations/kiln.dev~1release").and_then(Value::as_str).map(str::to_owned));
        if stamped.as_deref() != Some(r.id.to_string().as_str()) {
            return match deliver(d, r).await {
                Ok(()) => Ok((Landed::No, json!({ "application": app_name, "message": "written again" }))),
                Err(Undelivered::Refused(why)) => Ok((Landed::Refused(why), json!({ "application": app_name }))),
                Err(Undelivered::Unreachable(e)) => Err(e),
            };
        }
        let current = current.unwrap_or(Value::Null);
        return Ok(argocd::landed(&current, r.id, &version));
    }
    let cd = crate::cd::get().ok_or("KilnCD isn't running")?;
    let id = kilncd::app_id(&a, &env.name);
    let present = cd.apps.read().get(&id).map(|e| e.def.sources.iter().any(|s| s.as_ref.as_deref() == Some("release") && s.revision == commit));
    if !file.enabled {
        // KilnCD lets go of an app as it deletes it: one still there wasn't deleted, and is again.
        if cd.apps.read().contains_key(&id) {
            deliver(d, r).await.map_err(|e| e.to_string())?;
        }
        return Ok((if cd.apps.read().contains_key(&id) { Landed::No } else { Landed::Yes }, json!({ "app": id, "removed": true })));
    }
    if file.version.is_none() {
        return Ok((Landed::Yes, json!({ "app": id, "message": "registered and enabled; no version deployed yet" })));
    }
    if present != Some(true) {
        match deliver(d, r).await {
            Ok(()) => {}
            Err(Undelivered::Refused(why)) => return Ok((Landed::Refused(why), json!({ "app": id }))),
            Err(Undelivered::Unreachable(e)) => return Err(e),
        }
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

/// The trailer of a commit that reverts a release's: the release.
const REVERT_TRAILER: &str = "Kiln-Reverts";

/// What a release that couldn't land leaves in git: kiln's own commit is undone; a commit kiln
/// didn't make is someone's, and stays.
async fn give_back(d: &Deploy, r: &Release, why: &str) -> String {
    if r.origin == Origin::Git {
        return "it is a commit kiln didn't make, so git keeps it".to_owned();
    }
    undo(d, r, why).await
}

/// Puts git and the provider back after a release that won't land (refused, or cancelled while
/// delivering): its commit is reverted, and the provider handed what git says then. Nothing is
/// deleted on the way: a registration git says is disabled keeps its objects. What was done, as
/// the release's message says it.
async fn undo(d: &Deploy, r: &Release, why: &str) -> String {
    let (reverted, head) = match revert(d, r, why).await {
        Ok(back) => back,
        Err(e) => return format!("its commit couldn't be reverted ({e}): roll back to undo it"),
    };
    let said = match &reverted {
        Some(sha) => format!("its commit is reverted in {}", short(sha)),
        None => "nothing of its commit was left to revert".to_owned(),
    };
    let Some(head) = head else { return said };
    let back = Release { commit: Some(head.clone()), keep: true, ..r.clone() };
    if let Err(e) = deliver(d, &back).await {
        return format!("{said}, but handing the provider what git says failed: {e}");
    }
    let (a, env) = (store::app(&d.hub.store, &r.app).await.ok().flatten(), store::environment(&d.hub.store, &r.environment).await.ok().flatten());
    let file = match (&a, &env) {
        (Some(a), Some(env)) => release_file_at(d, a, env, &head).await.ok(),
        _ => None,
    };
    match file {
        Some(f) if f.enabled && f.version.is_some() => format!("{said}; release {} runs on", f.release),
        Some(_) => format!("{said}; nothing runs"),
        None => said,
    }
}

/// Reverts a release's commit: each file it changed goes back to what it was before, unless
/// someone changed it since. The revert's commit (`None`: nothing to revert), and the head after.
async fn revert(d: &Deploy, r: &Release, why: &str) -> Result<(Option<String>, Option<String>), String> {
    let Some(commit) = r.commit.clone() else { return Ok((None, None)) };
    let a = store::app(&d.hub.store, &r.app).await.map_err(|e| format!("{e:#}"))?.ok_or_else(|| format!("no app {}", r.app))?;
    let env = store::environment(&d.hub.store, &r.environment).await.map_err(|e| format!("{e:#}"))?.ok_or_else(|| format!("no environment {}", r.environment))?;
    let rr = env.release_repo.clone();
    let (hosted, at, key) = (d.hosted.clone(), rr.clone(), r.id.to_string());
    // Reverted already, before a restart.
    let found = crate::rt::blocking(move || hosted.find_trailer(&at.repo, &at.branch, REVERT_TRAILER, &key)).await.and_then(|x| x).map_err(|e| format!("{e:#}"))?;
    if found.is_some() {
        return Ok((found.clone(), found));
    }
    let paths = [repo::release_file(&rr, &a, &env.name), repo::values_file(&rr, &a, &env.name)];
    let (hosted, at) = (d.hosted.clone(), rr.clone());
    let (head, changes) = crate::rt::blocking(move || -> anyhow::Result<(Option<String>, Vec<Change>)> {
        let Some(head) = hosted.head(&at.repo, &at.branch)? else { return Ok((None, Vec::new())) };
        let mut changes = Vec::new();
        for path in paths {
            let ours = hosted.read(&at.repo, &commit, &path)?;
            // Someone changed it since: theirs stands.
            if hosted.read(&at.repo, &head, &path)? != ours {
                continue;
            }
            let before = hosted.read(&at.repo, &format!("{commit}^"), &path)?;
            if before != ours {
                changes.push(Change { path, content: before });
            }
        }
        Ok((Some(head), changes))
    })
    .await
    .and_then(|x| x)
    .map_err(|e| format!("{e:#}"))?;
    if changes.is_empty() {
        return Ok((None, head));
    }
    let message = format!("revert release {} of {} in {}\n\n{}\n\n{REVERT_TRAILER}: {}\n{}: kiln\n", r.id, a.name, env.name, crate::util::truncate(why.trim(), 600), r.id, repo::REQUESTED_BY_TRAILER);
    let sha = commit_files(d, &env, changes, message, "kiln").await?;
    Ok((Some(sha.clone()), Some(sha)))
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
        for c in commits.iter().filter(|c| c.trailer(repo::RELEASE_TRAILER).is_none() && c.trailer(REGISTRATION_TRAILER).is_none() && c.trailer(REVERT_TRAILER).is_none()) {
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
                    gates: Vec::new(),
                    approvals: Vec::new(),
                    promoted_from: None,
                    idempotency_key: None,
                    created_at: now,
                    committed_at: Some(now),
                    ended_at: None,
                };
                let (r, _) = store::insert_release(st, &r).await?;
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::authz::gate::Kind as CallerKind;

    fn env(agents_deploy: bool) -> Environment {
        Environment {
            name: "production".into(),
            provider: KILNCD.into(),
            release_repo: Default::default(),
            targets: Default::default(),
            gates: Default::default(),
            sync: Default::default(),
            watch: Default::default(),
            concurrency: Default::default(),
            agents: super::super::model::AgentPolicy { deploy: agents_deploy, approve: false },
        }
    }

    #[test]
    fn agents_release_only_where_the_environment_lets_them() {
        let agent = Who::of(&crate::authz::Caller::with(CallerKind::Agent, "agent:fix-it for user:ada", None, None));
        let person = Who::of(&crate::authz::Caller::with(CallerKind::User, "user:ada", None, None));
        assert!(agent.agent && !person.agent);
        assert!(matches!(agents_may(&agent, &env(false), "release apps"), Err((StatusCode::FORBIDDEN, m)) if m.contains("agents: { deploy: true }")));
        assert!(agents_may(&agent, &env(true), "release apps").is_ok());
        assert!(agents_may(&person, &env(false), "release apps").is_ok(), "a person releases anywhere the route table lets them");
    }
}
