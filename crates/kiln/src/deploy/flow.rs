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

use super::{appset, argocd};
use super::host::{Author, Change, CommitError};
use super::kilncd;
use super::provider::{Landed, Undelivered};
use super::gates::{self, Verdict};
use super::model::{App, Approval, Artifact, Concurrency, Contacts, Environment, KILN_HOST, KILNCD, Kind, Origin, Package, Registration, Release, ReleaseFile, State, Template};
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
pub(super) fn emit_release(d: &Deploy, r: &Release) {
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
    // Chat, paging and webhooks: the server's rules, and the app's own channels.
    let event = match r.state {
        State::WaitingApproval => Some("release.waiting_approval"),
        State::Succeeded => Some("release.succeeded"),
        State::Failed => Some("release.failed"),
        State::Cancelled => Some("release.cancelled"),
        _ => None,
    };
    if let Some(event) = event {
        let (hub, app) = (Arc::clone(&d.hub), r.app.clone());
        let mut release = serde_json::to_value(r).unwrap_or(Value::Null);
        if let (Some(m), Some(approvers)) = (release.as_object_mut(), e.get("approvers")) {
            m.insert("approvers".into(), approvers.clone());
        }
        crate::rt::spawn(async move {
            let channels = store::app(&hub.store, &app).await.ok().flatten().map(|a| a.contacts.channels).unwrap_or_default();
            crate::ci::notify::release_event(hub, event, release, channels).await;
        });
    }
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

/// Refused while an environment moves between providers (§8.4): what runs there is changing hands.
async fn not_moving(d: &Deploy, env: &str) -> Result<(), Fail> {
    match super::moves::under_way(d, env).await.map_err(internal)? {
        Some(m) => Err(conflict(format!("{env} is moving to {}: releases there wait until the move is done or cancelled", m.to))),
        None => Ok(()),
    }
}

/// Follows an Argo CD's Applications by its stream from now on, unless kiln already does: each
/// change wakes the release of its registration in flight.
fn watch_argo(d: &Deploy, integration: &str) {
    let changed: argocd::Changed = Arc::new(|app: &Value| {
        let label = |k: &str| app.pointer(&format!("/metadata/labels/{}", k.replace('/', "~1"))).and_then(Value::as_str).map(str::to_owned);
        let (Some(a), Some(e), Some(d)) = (label("kiln.dev/app"), label("kiln.dev/environment"), super::get()) else { return };
        crate::rt::spawn(async move {
            if let Ok(Some(r)) = store::in_flight(&d.hub.store, &a, &e).await
                && r.state == State::Delivering
                && store::schedule(&d.hub.store, r.id, crate::util::now_unix()).await.is_ok()
            {
                d.wake.notify();
            }
        });
    });
    d.argo.ensure(&d.hub, integration, changed);
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
    for (provider, place) in &e.targets.platforms {
        if argo_of(&e).is_some() {
            return Err(bad(format!("Argo CD environments deploy to Kubernetes only: {provider} is a KilnCD environment's target")));
        }
        let (target, _) = kiln_cd::providers::target_of(provider, place).map_err(|m| bad(format!("targets.platforms.{provider}: {m}")))?;
        let Some(def) = kiln_cd::providers::get(provider) else { continue };
        if !def.built {
            return Err(bad(format!("{} isn't built yet: kiln lists it, and deploys to it once it is", def.title)));
        }
        // Its account is an integration of the provider's kind (a runner's targets need none).
        if def.integration != "runner" {
            match crate::ci::integrations::get(&d.hub, &target.account).await.map_err(internal)? {
                Some(i) if i.kind == def.integration => {}
                Some(i) => return Err(bad(format!("targets.platforms.{provider}: {} is an integration of kind {}, and {} deploys through one of kind {}", target.account, i.kind, def.title, def.integration))),
                None => return Err(bad(format!("targets.platforms.{provider}: no integration `{}` (Settings → Integrations): {} deploys through an {} account", target.account, def.title, def.integration))),
            }
        }
    }
    if e.watch.timeout_secs < 30 || e.watch.settle_secs > e.watch.timeout_secs || e.watch.refused_secs > e.watch.timeout_secs {
        return Err(bad("watch: timeout_secs at least 30, settle_secs and refused_secs at most timeout_secs"));
    }
    let before = store::environment(&d.hub.store, &e.name).await.map_err(internal)?;
    // Its provider changes by a move, which hands what runs over (§8.4).
    if let Some(b) = &before
        && b.provider != e.provider
        && !store::registrations(&d.hub.store, None, Some(&e.name)).await.map_err(internal)?.is_empty()
    {
        return Err(conflict(format!(
            "{} has apps registered on {}: move it (POST /api/deploy/environments/{}/move, `kiln deploy env move`), which hands what runs to {} with nothing made again",
            e.name, b.provider, e.name, e.provider
        )));
    }
    ensure_release_repo(d, &e.release_repo).await.map_err(bad)?;
    // On Argo CD: its ApplicationSet, and what it needs, in place before the environment says so
    // (or none, where kiln writes the Applications). Only what they read asks Argo CD: a freeze or a
    // gate changes while it's away.
    let argo_reads = before.as_ref().is_none_or(|b| b.provider != e.provider || b.release_repo != e.release_repo || b.targets != e.targets || b.sync != e.sync || b.argocd != e.argocd);
    if argo_reads && let Some(name) = argo_of(&e) {
        let argo = argocd::Argo::of(&d.hub, name).await.map_err(|why| (StatusCode::BAD_GATEWAY, why))?;
        match appset::integration_of(&e) {
            Some(_) => appset::ensure(d, &argo, &e).await.map_err(|why| (StatusCode::BAD_GATEWAY, why.to_string()))?,
            // kiln takes to writing the Applications: their ApplicationSet goes, and they stay
            // (kiln's ApplicationSets never delete theirs).
            None if before.as_ref().is_some_and(|b| appset::integration_of(b) == Some(name)) => {
                argo.delete_application_set(&argocd::set_name(&e.name)).await.map_err(|why| (StatusCode::BAD_GATEWAY, why))?;
            }
            None => {}
        }
    }
    store::put_environment(&d.hub.store, &e).await.map_err(internal)?;
    // Its gates may have changed: what waits on them looks again now.
    let held = store::releases(&d.hub.store, store::ReleaseQuery { app: None, environment: Some(e.name.clone()), state: None, version: None, limit: 1000 }).await.map_err(internal)?;
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
    let env = environment(d, name).await?;
    // Its ApplicationSet goes with it (it makes nothing: nothing is registered).
    if let Some(integration) = appset::integration_of(&env) {
        let gone = match argocd::Argo::of(&d.hub, integration).await {
            Ok(argo) => argo.delete_application_set(&argocd::set_name(name)).await,
            Err(e) => Err(e),
        };
        if let Err(why) = gone {
            tracing::warn!(environment = %name, error = %why, "deploy: an environment's ApplicationSet couldn't be deleted with it");
        }
    }
    if !store::delete_environment(&d.hub.store, name).await.map_err(internal)? {
        return Err(missing(format!("no environment {name}")));
    }
    audit(d, actor, "deploy.environment.delete", name.to_owned()).await;
    // Its apps' secrets there go with it (§3.5).
    let secrets = crate::secrets::of(&d.hub);
    for a in store::apps(&d.hub.store).await.map_err(internal)? {
        secrets.forget(&crate::secrets::Scope::of(&a.name, name), actor).await.map_err(internal)?;
    }
    Ok(())
}

async fn environment(d: &Deploy, name: &str) -> Result<Environment, Fail> {
    store::environment(&d.hub.store, name).await.map_err(internal)?.ok_or_else(|| missing(format!("no environment {name}")))
}

// ------------------------------------------------------------------ apps

pub async fn put_app(d: &Deploy, actor: &str, a: App) -> Result<App, Fail> {
    check_app(d, &a).await?;
    store::put_app(&d.hub.store, &a).await.map_err(internal)?;
    audit(d, actor, "deploy.app.put", a.name.clone()).await;
    Ok(a)
}

/// What an app must be to be kept: its names DNS labels, a package that deploys, what its template
/// allows, a source repository kiln has.
async fn check_app(d: &Deploy, a: &App) -> Result<(), Fail> {
    dns_label("app", &a.name)?;
    dns_label("project", &a.project)?;
    if let Some(ns) = &a.namespace {
        dns_label("namespace", ns)?;
    }
    match &a.package {
        super::model::Package::Helm { repo, chart, .. } if repo.is_empty() || chart.is_empty() => return Err(bad("a Helm package names its repository and its chart")),
        super::model::Package::Git { repo, .. } if repo.is_empty() => return Err(bad("a git package names its repository")),
        super::model::Package::Platform { provider, spec, .. } => {
            match kiln_cd::providers::get(provider) {
                None => return Err(bad(format!("kiln deploys to no platform {provider} (aws-ecs, aws-lambda, cloudflare-workers, vercel)"))),
                Some(def) if !def.built => return Err(bad(format!("{} isn't built yet: kiln lists it, and deploys to it once it is", def.title))),
                Some(_) => {}
            }
            if !spec.is_object() {
                return Err(bad(format!("a {provider} package's spec is its definition: an object, as its key in .kiln/deploy.yaml")));
            }
            repo::check_value(spec, repo::PLATFORM_KEEPS).map_err(|e| bad(format!("spec: {e}")))?;
        }
        _ => {}
    }
    for (k, v) in a.package.set() {
        repo::check(v).map_err(|e| bad(format!("set {k}: {e}")))?;
    }
    if let Some(name) = &a.template {
        let t = template(d, name).await?;
        if let Some(pattern) = &t.namespace {
            let wanted = pattern.replace("{app}", &a.name);
            if a.namespace.as_deref().is_some_and(|n| n != wanted) {
                return Err(bad(format!("template {name} runs its apps in {pattern}: {} runs in {wanted}", a.name)));
            }
        }
        for (k, v) in &a.properties {
            match t.properties.get(k) {
                None if !t.properties.is_empty() => return Err(bad(format!("template {name} has no property {k} ({})", t.properties.keys().cloned().collect::<Vec<_>>().join(", ")))),
                Some(allowed) if !allowed.is_empty() && !allowed.contains(v) => return Err(bad(format!("{k} is one of {} (template {name})", allowed.join(", ")))),
                _ => {}
            }
        }
    }
    if let Some(s) = &a.source {
        let repos = d.hub.store.repos().await.map_err(internal)?;
        if !repos.iter().any(|r| &r.name == s) {
            return Err(bad(format!("source {s}: kiln has no repository {s}")));
        }
    }
    Ok(())
}

/// Deletes an app. `everywhere`: deregistered from every environment first, what it runs there
/// deleted, or kept with `keep`; otherwise an app still registered somewhere stays.
pub async fn delete_app(d: &Deploy, who: &Who, name: &str, everywhere: bool, keep: bool) -> Result<(), Fail> {
    let actor = who.name.as_str();
    let regs = store::registrations(&d.hub.store, Some(name), None).await.map_err(internal)?;
    if everywhere {
        for r in &regs {
            deregister(d, who, name, &r.environment, keep).await?;
        }
    } else if !regs.is_empty() {
        let envs: Vec<&str> = regs.iter().map(|r| r.environment.as_str()).collect();
        return Err(conflict(format!("{name} is registered in {}. Deregister it first, or delete it everywhere", envs.join(", "))));
    }
    if !store::delete_app(&d.hub.store, name).await.map_err(internal)? {
        return Err(missing(format!("no app {name}")));
    }
    audit(d, actor, "deploy.app.delete", name.to_owned()).await;
    // Its secrets go with it (§3.5): an app made later with its name doesn't get them.
    let secrets = crate::secrets::of(&d.hub);
    for e in store::environments(&d.hub.store).await.map_err(internal)? {
        secrets.forget(&crate::secrets::Scope::of(name, &e.name), actor).await.map_err(internal)?;
    }
    Ok(())
}

async fn app(d: &Deploy, name: &str) -> Result<App, Fail> {
    store::app(&d.hub.store, name).await.map_err(internal)?.ok_or_else(|| missing(format!("no app {name}")))
}

/// An app as asked for (`POST /apps`): its template may give its package and its namespace.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AppAsk {
    pub name: String,
    #[serde(default)]
    pub project: Option<String>,
    #[serde(default)]
    pub owners: Vec<String>,
    #[serde(default)]
    pub package: Option<Package>,
    #[serde(default)]
    pub namespace: Option<String>,
    #[serde(default)]
    pub properties: BTreeMap<String, String>,
    #[serde(default)]
    pub template: Option<String>,
    #[serde(default)]
    pub contacts: Contacts,
    /// Its source repository, one kiln has (`acme/web`), or `""` for none. Unset: the one it has,
    /// or else the one its template makes (§3.1).
    #[serde(default)]
    pub source: Option<String>,
    #[serde(default)]
    pub archived: bool,
}

/// Creates (or replaces) an app from what was asked: what its template gives fills what wasn't.
pub async fn create_app(d: &Deploy, actor: &str, ask: AppAsk) -> Result<App, Fail> {
    let t = match &ask.template {
        Some(name) => Some(template(d, name).await?),
        None => None,
    };
    let from_template = || -> Result<Option<Package>, Fail> {
        let Some(p) = t.as_ref().and_then(|t| t.package.clone()) else { return Ok(None) };
        // `{app}` in the template's package is the app's name.
        let v = fill_app(serde_json::to_value(&p).map_err(|e| internal(e.into()))?, &ask.name);
        serde_json::from_value(v).map(Some).map_err(|e| internal(e.into()))
    };
    let package = match ask.package.clone() {
        Some(p) => p,
        None => from_template()?.ok_or_else(|| bad("how is it deployed? give a package, or a template that has one"))?,
    };
    let namespace = ask.namespace.clone().or_else(|| t.as_ref().and_then(|t| t.namespace.as_ref()).map(|p| p.replace("{app}", &ask.name)));
    let had = store::app(&d.hub.store, &ask.name).await.map_err(internal)?.and_then(|a| a.source);
    let mut a = App {
        name: ask.name,
        project: ask.project.unwrap_or_else(|| "default".to_owned()),
        owners: ask.owners,
        package,
        namespace,
        properties: ask.properties,
        template: ask.template,
        contacts: ask.contacts,
        source: None,
        archived: ask.archived,
    };
    check_app(d, &a).await?;
    // Its source repository: the one named, the one it has, or one its template makes, once the
    // rest of it is known to be right.
    let mut made = None;
    a.source = match (ask.source, had, t.as_ref().and_then(|t| t.repository.as_ref())) {
        (Some(s), _, _) => Some(s).filter(|s| !s.is_empty()),
        (None, Some(s), _) => Some(s),
        (None, None, Some(r)) => {
            made = Some(super::scaffold::create(d, r, &a, actor).await?);
            made.clone()
        }
        (None, None, None) => None,
    };
    put_app(d, actor, a).await.map_err(|(code, why)| match &made {
        Some(r) => (code, format!("{why} (its repository {r} is made: give it as the app's source)")),
        None => (code, why),
    })
}

/// `{app}` in every string of a value is `name`.
fn fill_app(v: Value, name: &str) -> Value {
    match v {
        Value::String(s) => Value::String(s.replace("{app}", name)),
        Value::Array(a) => Value::Array(a.into_iter().map(|x| fill_app(x, name)).collect()),
        Value::Object(m) => Value::Object(m.into_iter().map(|(k, x)| (k, fill_app(x, name))).collect()),
        other => other,
    }
}

/// What changes in an app (`PATCH /apps/{app}`): only what is given.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AppPatch {
    #[serde(default)]
    pub project: Option<String>,
    #[serde(default)]
    pub owners: Option<Vec<String>>,
    #[serde(default)]
    pub package: Option<Package>,
    #[serde(default)]
    pub namespace: Option<String>,
    /// Set (a value) or removed (`null`), each.
    #[serde(default)]
    pub properties: BTreeMap<String, Option<String>>,
    #[serde(default)]
    pub template: Option<String>,
    #[serde(default)]
    pub contacts: Option<Contacts>,
    /// A repository kiln has, or `""` for none.
    #[serde(default)]
    pub source: Option<String>,
    #[serde(default)]
    pub archived: Option<bool>,
}

/// Changes what a patch gives of an app; the package's change reaches each environment with its
/// next release.
pub async fn patch_app(d: &Deploy, actor: &str, name: &str, p: AppPatch) -> Result<App, Fail> {
    let mut a = app(d, name).await?;
    if let Some(x) = p.project {
        a.project = x;
    }
    if let Some(x) = p.owners {
        a.owners = x;
    }
    if let Some(x) = p.package {
        a.package = x;
    }
    if let Some(x) = p.namespace {
        a.namespace = Some(x).filter(|n| !n.is_empty());
    }
    for (k, v) in p.properties {
        match v {
            Some(v) => a.properties.insert(k, v),
            None => a.properties.remove(&k),
        };
    }
    if let Some(x) = p.template {
        a.template = Some(x).filter(|t| !t.is_empty());
    }
    if let Some(x) = p.contacts {
        a.contacts = x;
    }
    if let Some(x) = p.source {
        a.source = Some(x).filter(|s| !s.is_empty());
    }
    if let Some(x) = p.archived {
        a.archived = x;
    }
    put_app(d, actor, a).await
}

/// Whether `name` is free for a new app, and why not.
pub async fn app_name_free(d: &Deploy, name: &str) -> Result<Value, Fail> {
    if let Err((_, why)) = dns_label("app", name) {
        return Ok(json!({ "name": name, "available": false, "why": why }));
    }
    let taken = store::app(&d.hub.store, name).await.map_err(internal)?.is_some();
    Ok(json!({ "name": name, "available": !taken, "why": if taken { Some(format!("{name} is an app already")) } else { None } }))
}

/// The app as Backstage's catalog reads it (`catalog-info.yaml`).
pub fn catalog_info(a: &App) -> String {
    let links: Vec<Value> = a.contacts.links.iter().map(|(title, url)| json!({ "title": title, "url": url })).collect();
    let mut annotations = serde_json::Map::new();
    annotations.insert("kiln.dev/app".into(), json!(a.name));
    annotations.insert("kiln.dev/project".into(), json!(a.project));
    if let Some(o) = &a.contacts.oncall {
        annotations.insert("kiln.dev/oncall".into(), json!(o));
    }
    if !a.contacts.channels.is_empty() {
        annotations.insert("kiln.dev/channels".into(), json!(a.contacts.channels.join(",")));
    }
    if let Some(s) = &a.source {
        annotations.insert("kiln.dev/source".into(), json!(s));
    }
    let doc = json!({
        "apiVersion": "backstage.io/v1alpha1",
        "kind": "Component",
        "metadata": { "name": a.name, "annotations": annotations, "links": links, "tags": a.template.iter().collect::<Vec<_>>() },
        "spec": {
            "type": a.template.clone().unwrap_or_else(|| "service".to_owned()),
            "lifecycle": if a.archived { "deprecated" } else { "production" },
            "owner": a.owners.first().cloned().unwrap_or_else(|| "unknown".to_owned()),
            "system": a.project,
        },
    });
    serde_yaml::to_string(&doc).unwrap_or_default()
}

// ------------------------------------------------------------------ templates

async fn template(d: &Deploy, name: &str) -> Result<Template, Fail> {
    store::template(&d.hub.store, name).await.map_err(internal)?.ok_or_else(|| missing(format!("no template {name}")))
}

/// Puts a template. One that makes repositories, or changes how, is put by someone who may add
/// repositories (`adds_repos`: `settings.manage`), since each app made from it makes one.
pub async fn put_template(d: &Deploy, actor: &str, t: Template, adds_repos: bool) -> Result<Template, Fail> {
    dns_label("template", &t.name)?;
    if let Some(r) = &t.repository {
        let before = store::template(&d.hub.store, &t.name).await.map_err(internal)?.and_then(|b| b.repository);
        if !adds_repos && before.as_ref() != Some(r) {
            return Err((StatusCode::FORBIDDEN, "a template that makes repositories is put by someone who may add them (settings.manage)".into()));
        }
        super::scaffold::check(d, r).map_err(bad)?;
    }
    for c in &t.checks {
        if !matches!(c.as_str(), "no-snapshots" | "owners-set") {
            return Err(bad(format!("kiln has no check `{c}` (no-snapshots, owners-set)")));
        }
    }
    if let Some(p) = &t.package {
        for (k, v) in p.set() {
            repo::check(v).map_err(|e| bad(format!("package.set {k}: {e}")))?;
        }
    }
    store::put_template(&d.hub.store, &t).await.map_err(internal)?;
    audit(d, actor, "deploy.template.put", t.name.clone()).await;
    Ok(t)
}

pub async fn delete_template(d: &Deploy, actor: &str, name: &str) -> Result<(), Fail> {
    let using: Vec<String> = store::apps(&d.hub.store).await.map_err(internal)?.into_iter().filter(|a| a.template.as_deref() == Some(name)).map(|a| a.name).collect();
    if !using.is_empty() {
        return Err(conflict(format!("apps are made from {name}: {}", using.join(", "))));
    }
    if !store::delete_template(&d.hub.store, name).await.map_err(internal)? {
        return Err(missing(format!("no template {name}")));
    }
    audit(d, actor, "deploy.template.delete", name.to_owned()).await;
    Ok(())
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
    not_moving(d, env_name).await?;
    let (a, env) = (app(d, app_name).await?, environment(d, env_name).await?);
    if a.archived {
        return Err(conflict(format!("{app_name} is archived")));
    }
    if store::registration(&d.hub.store, app_name, env_name).await.map_err(internal)?.is_some() {
        return Err(conflict(format!("{app_name} is registered in {env_name} already")));
    }
    if let Some(name) = &a.template {
        let t = template(d, name).await?;
        if !t.environments.is_empty() && !t.environments.iter().any(|e| e == env_name) {
            return Err(conflict(format!("template {name} keeps its apps to {}", t.environments.join(", "))));
        }
    }
    if let super::model::Package::Platform { provider, .. } = &a.package {
        if argo_of(&env).is_some() {
            return Err(conflict(format!("{app_name} deploys to {provider}, and {env_name} is on Argo CD, which deploys to Kubernetes only: register it in a KilnCD environment")));
        }
        if !env.targets.platforms.contains_key(provider) {
            return Err(conflict(format!("{env_name} has no {provider} target: give it targets.platforms.{provider}: {{ account: <integration>, … }} first")));
        }
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
    let reg = Registration { app: app_name.to_owned(), environment: env_name.to_owned(), enabled: false, paused: false, running: None, created_at: crate::util::now_unix(), degraded: None };
    store::put_registration(&d.hub.store, &reg).await.map_err(internal)?;
    // On Argo CD, its project allows it from now (the first release does, if Argo CD is away now).
    if let Some(integration) = appset::integration_of(&env) {
        let ready = match argocd::Argo::of(&d.hub, integration).await {
            Ok(argo) => appset::ensure(d, &argo, &env).await.map_err(|e| e.to_string()),
            Err(e) => Err(e),
        };
        if let Err(why) = ready {
            tracing::warn!(app = %app_name, environment = %env_name, error = %why, "deploy: Argo CD isn't ready for a registration yet; its first release makes it so");
        }
    }
    audit(d, actor, "deploy.register", format!("{app_name} in {env_name}")).await;
    d.hub.emit(&json!({ "type": "deploy_registration", "registration": reg }));
    Ok(reg)
}

pub async fn deregister(d: &Deploy, who: &Who, app_name: &str, env_name: &str, keep: bool) -> Result<(), Fail> {
    not_moving(d, env_name).await?;
    let actor = who.name.as_str();
    let (a, env) = (app(d, app_name).await?, environment(d, env_name).await?);
    agents_may(who, &env, "deregister apps")?;
    let reg = registration(d, app_name, env_name).await?;
    if let Some(r) = store::in_flight(&d.hub.store, app_name, env_name).await.map_err(internal)? {
        return Err(conflict(format!("release {} is in flight for {app_name} in {env_name}: wait for it, or cancel it", r.id)));
    }
    let runs = reg.running.is_some() || reg.enabled;
    // Through an ApplicationSet, its files go first: deleted before, its Application would be made
    // again from them.
    let through_set = appset::integration_of(&env).is_some();
    if runs && !through_set {
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
    if runs
        && through_set
        && let Some(name) = argo_of(&env)
    {
        let argo = argocd::Argo::of(&d.hub, name).await.map_err(|e| (StatusCode::BAD_GATEWAY, e))?;
        appset::remove(d, &argo, &env, &argocd::application_name(&a, env_name), keep).await.map_err(|e| (StatusCode::BAD_GATEWAY, e))?;
    }
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
    not_moving(d, env_name).await?;
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
    /// A secrets release (§3.5): the app's secrets at their versions now, its version and values
    /// kept. What changed, in a few words (`DB_PASSWORD v4`).
    #[serde(default)]
    pub secrets: Option<String>,
    /// Failed, it isn't reverted: it's held for a person (§8.1). For a change a revert would undo
    /// harmfully: a secret rotated after a leak, a migration that can't run backwards.
    #[serde(default)]
    pub no_revert: bool,
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RollbackAsk {
    /// The release to go back to; by default the newest that succeeded before the one running.
    #[serde(default)]
    pub to: Option<i64>,
}

/// A change to many apps of an environment at once (docs/deploy-api.md §3.3): each app selected is
/// released, at one version, or with values set in its own (a config release each). Each passes
/// the environment's gates as any release does.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FleetAsk {
    pub environment: String,
    /// The apps: these, or those made from `template`, in `project`, owned by `owner` (each given
    /// must hold); every app registered in the environment when none is given.
    #[serde(default)]
    pub apps: Vec<String>,
    #[serde(default)]
    pub template: Option<String>,
    #[serde(default)]
    pub project: Option<String>,
    #[serde(default)]
    pub owner: Option<String>,
    /// A deploy of this version to each.
    #[serde(default)]
    pub version: Option<String>,
    /// Values set in each app's own values in the environment, by path (`image.pullPolicy`):
    /// numbers and `true`/`false` as such, the rest as text.
    #[serde(default)]
    pub set: BTreeMap<String, String>,
    /// Asked for again with the same key, it is the same fleet: the same releases.
    #[serde(default)]
    pub idempotency_key: Option<String>,
}

/// `set`'s values in a values file, by path; the file as YAML (its comments don't survive).
pub fn merge_values(text: &str, set: &BTreeMap<String, String>) -> Result<String, String> {
    let mut doc: serde_yaml::Value = if text.trim().is_empty() { serde_yaml::Value::Mapping(serde_yaml::Mapping::new()) } else { serde_yaml::from_str(text).map_err(|e| format!("its values don't read: {e}"))? };
    if doc.is_null() {
        doc = serde_yaml::Value::Mapping(serde_yaml::Mapping::new());
    }
    for (path, raw) in set {
        let value = match serde_yaml::from_str::<serde_yaml::Value>(raw) {
            Ok(v @ (serde_yaml::Value::Number(_) | serde_yaml::Value::Bool(_))) => v,
            _ => serde_yaml::Value::String(raw.clone()),
        };
        let parts: Vec<&str> = path.split('.').collect();
        if parts.iter().any(|p| p.is_empty()) {
            return Err(format!("`{path}` isn't a path of values (`image.tag`)"));
        }
        let mut at = &mut doc;
        for (i, p) in parts.iter().enumerate() {
            let above = parts.get(..i).map(|x| x.join(".")).unwrap_or_default();
            let map = at.as_mapping_mut().ok_or_else(|| format!("{path}: {above} isn't a map of values"))?;
            let key = serde_yaml::Value::String((*p).to_owned());
            if i + 1 == parts.len() {
                map.insert(key, value.clone());
                break;
            }
            at = map.entry(key).or_insert_with(|| serde_yaml::Value::Mapping(serde_yaml::Mapping::new()));
        }
    }
    serde_yaml::to_string(&doc).map_err(|e| format!("{e}"))
}

/// Releases each selected app of a fleet change (see [`FleetAsk`]): its id, and each app's release,
/// or why it has none.
pub async fn fleet(d: &Deploy, who: &Who, f: FleetAsk) -> Result<Value, Fail> {
    if f.version.is_some() == !f.set.is_empty() {
        return Err(bad("a fleet change is a version to deploy, or values to set: one of them"));
    }
    let env = store::environment(&d.hub.store, &f.environment).await.map_err(internal)?.ok_or_else(|| missing(format!("no environment {}", f.environment)))?;
    agents_may(who, &env, "release apps")?;
    let id = f.idempotency_key.clone().unwrap_or_else(crate::util::new_id);
    let registered: Vec<String> = store::registrations(&d.hub.store, None, Some(&env.name)).await.map_err(internal)?.into_iter().map(|r| r.app).collect();
    let apps: Vec<App> = store::apps(&d.hub.store)
        .await
        .map_err(internal)?
        .into_iter()
        .filter(|a| registered.contains(&a.name))
        .filter(|a| f.apps.is_empty() || f.apps.contains(&a.name))
        .filter(|a| f.template.as_ref().is_none_or(|t| a.template.as_ref() == Some(t)))
        .filter(|a| f.project.as_ref().is_none_or(|p| &a.project == p))
        .filter(|a| f.owner.as_ref().is_none_or(|o| a.owners.contains(o)))
        .collect();
    if apps.is_empty() {
        return Err(bad(format!("no app registered in {} is selected", env.name)));
    }
    let mut out = Vec::new();
    for a in &apps {
        let key = Some(format!("fleet:{id}:{}", a.name));
        let ask = match &f.version {
            Some(v) => Ask { app: a.name.clone(), environment: env.name.clone(), version: Some(v.clone()), idempotency_key: key, ..Ask::default() },
            None => {
                let values = match current_values(d, a, &env).await.and_then(|text| merge_values(&text.unwrap_or_default(), &f.set).map_err(|e| (StatusCode::UNPROCESSABLE_ENTITY, e))) {
                    Ok(v) => v,
                    Err((code, why)) => {
                        out.push(json!({ "app": a.name, "status": code.as_u16(), "error": why }));
                        continue;
                    }
                };
                Ask { app: a.name.clone(), environment: env.name.clone(), values: Some(values), idempotency_key: key, ..Ask::default() }
            }
        };
        match release(d, who, ask).await {
            Ok(r) => out.push(json!({ "app": a.name, "release": r })),
            Err((code, why)) => out.push(json!({ "app": a.name, "status": code.as_u16(), "error": why })),
        }
    }
    audit(d, &who.name, "deploy.fleet", format!("{id} in {}: {} apps", env.name, apps.len())).await;
    Ok(json!({ "fleet": id, "environment": env.name, "releases": out }))
}

/// A fleet change's releases as they stand, and whether each has finished.
pub async fn fleet_status(d: &Deploy, id: &str) -> Result<Value, Fail> {
    let list = store::releases_keyed(&d.hub.store, &format!("fleet:{id}:")).await.map_err(internal)?;
    if list.is_empty() {
        return Err(missing(format!("no fleet change {id}")));
    }
    let done = list.iter().all(|r| !r.state.in_flight());
    let mut states: BTreeMap<&str, u32> = BTreeMap::new();
    for r in &list {
        *states.entry(r.state.as_str()).or_default() += 1;
    }
    Ok(json!({ "fleet": id, "done": done, "states": states, "releases": list }))
}

/// The values file an app runs with in an environment (`None`: nothing runs, or it has none).
async fn current_values(d: &Deploy, a: &App, env: &Environment) -> Result<Option<String>, Fail> {
    let reg = store::registration(&d.hub.store, &a.name, &env.name).await.map_err(internal)?;
    let running = match reg.and_then(|r| r.running) {
        Some(id) => store::release(&d.hub.store, id).await.map_err(internal)?,
        None => None,
    };
    let Some(commit) = running.and_then(|r| r.commit) else { return Ok(None) };
    file_at(d, env, &commit, repo::values_file(&env.release_repo, a, &env.name)).await.map_err(|e| (StatusCode::BAD_GATEWAY, e))
}

/// The releases a registration can roll back to: the last 10 that succeeded, the one running and
/// any rolled back from left out (as txst's rule).
pub async fn candidates(d: &Deploy, app_name: &str, env_name: &str) -> Result<Vec<Release>, Fail> {
    let reg = registration(d, app_name, env_name).await?;
    let all = store::releases(&d.hub.store, store::ReleaseQuery { app: Some(app_name.into()), environment: Some(env_name.into()), state: None, version: None, limit: 500 }).await.map_err(internal)?;
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
    not_moving(d, &env.name).await?;
    let asked = [ask.version.is_some(), ask.values.is_some(), ask.rollback.is_some(), ask.enabled.is_some(), ask.promote_from.is_some(), ask.secrets.is_some()].iter().filter(|b| **b).count();
    if asked != 1 {
        return Err(bad("ask for exactly one of: a version, values, a rollback, enabled, promote_from, or secrets"));
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
        secrets: None,
        no_revert: ask.no_revert,
        reverts: None,
        reverted_by: None,
        irreversible: None,
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
    } else if let Some(what) = ask.secrets {
        r.kind = Kind::Config;
        r.secrets = Some(Some(what.trim()).filter(|w| !w.is_empty()).unwrap_or("as they are").to_owned());
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
    let unreachable = env.as_ref().map_or(6 * 60 * 60, |e| i64::try_from(e.watch.unreachable_secs).unwrap_or(21_600)).max(timeout);
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
            // What can be known to fail is refused while nothing has changed (§8.1). A rollback
            // refused leaves running what it rolled back from: a person is needed.
            Ok(Committed::Refused(why)) => {
                let message = format!("refused before its commit, nothing changed: {why}");
                if r.kind == Kind::Rollback {
                    degrade(d, &r, &format!("release {} failed: {message}; what it rolled back from runs on", r.id)).await?;
                }
                finish(d, r, State::Failed, Some(message)).await?;
            }
            Ok(Committed::Done(sha, secrets, irreversible)) => {
                r.irreversible = irreversible;
                r.commit = Some(sha);
                // What it changes of the app's secrets, as it pinned them (§3.5).
                if let Some(changed) = secrets.filter(|c| !c.is_empty()) {
                    r.secrets = Some(changed);
                }
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
                // A release that fails after its commit is reverted, as a release of its own,
                // or held for a person (§8.1): see `aftermath`.
                Ok((Landed::Failed(why), view)) => {
                    r.provider = view;
                    failed(d, r, why).await?;
                }
                Ok((Landed::Refused(why), mut view)) => {
                    let since = r.provider.get("refused_at").and_then(Value::as_i64).unwrap_or(now);
                    let grace = env.as_ref().map_or(60, |e| i64::try_from(e.watch.refused_secs).unwrap_or(60));
                    let name = env.as_ref().map_or("the provider", provider_name);
                    note(&mut view, "refused_at", json!(since));
                    r.provider = view;
                    if now - since >= grace {
                        return refused(d, r, format!("{name} refused it: {why}")).await;
                    }
                    r.message = Some(format!("{name} refuses it: {why} (it fails in {}s unless that clears)", grace - (now - since)));
                    store::update_release(st, &r, State::Delivering).await?;
                    store::schedule(st, id, now + 5).await?;
                }
                // Evidence it doesn't run: failed, unless it recovers within `watch.degraded`.
                Ok((Landed::Degraded(why), mut view)) => {
                    let since = r.provider.get("degraded_at").and_then(Value::as_i64).unwrap_or(now);
                    let grace = env.as_ref().map_or(60, |e| i64::try_from(e.watch.degraded_secs).unwrap_or(60));
                    note(&mut view, "degraded_at", json!(since));
                    r.provider = view;
                    if now - since >= grace {
                        // Said object by object where the provider says it (Kubernetes' reason).
                        let why = degraded_objects(d, &r).await.unwrap_or(why);
                        return failed(d, r, format!("it is Degraded: {why}")).await;
                    }
                    r.message = Some(format!("Degraded: {why} (it fails in {}s unless it recovers)", grace - (now - since)));
                    store::update_release(st, &r, State::Delivering).await?;
                    store::schedule(st, id, now + 5).await?;
                }
                // Still progressing as its provider sees it (Kubernetes hasn't called the rollout
                // stuck): a slow rollout isn't a failed one. It waits, up to three times its timeout.
                Ok((Landed::No, view)) if now - committed_at > timeout && now - committed_at <= timeout * 3 && view.get("health").and_then(Value::as_str) == Some("Progressing") => {
                    r.provider = view;
                    r.message = Some(format!("still progressing past its {timeout}s; it waits up to {}s while it does", timeout * 3));
                    store::update_release(st, &r, State::Delivering).await?;
                    store::schedule(st, id, now + 5).await?;
                }
                Ok((Landed::No, view)) if now - committed_at > timeout => {
                    let progressing = view.get("health").and_then(Value::as_str) == Some("Progressing");
                    r.provider = view;
                    let why = if progressing { format!("it didn't land within {}s, still progressing (three times watch.timeout_secs)", timeout * 3) } else { format!("it didn't land within {timeout}s (watch.timeout_secs)") };
                    failed(d, r, why).await?;
                }
                Ok((Landed::No, view)) => {
                    // What it waits on, where the provider says; a stream wakes it as that changes,
                    // so it looks less often by itself.
                    let watched = view.get("watched").and_then(Value::as_bool) == Some(true);
                    r.message = view.get("waiting").and_then(Value::as_str).map(|w| format!("waiting for {w}"));
                    r.provider = view;
                    store::update_release(st, &r, State::Delivering).await?;
                    store::schedule(st, id, now + if watched { 15 } else { 3 }).await?;
                }
                // The provider never held it: nothing changed there, and git goes back too.
                Err(e) if now - committed_at > timeout && delivered.is_none() => {
                    refused(d, r, format!("it never reached the provider within {timeout}s: {e}")).await?;
                }
                // It held it, and can't be asked since: what it did is unknown, and nothing is
                // done blind. It's judged when the provider answers; past `unreachable_secs`, held.
                Err(e) if now - committed_at > unreachable => {
                    hold(d, r, &format!("the provider couldn't be asked for {unreachable}s: {e}"), "what it did is unknown").await?;
                }
                Err(e) if now - committed_at > timeout => {
                    r.message = Some(format!("the provider can't be asked ({e}): it is judged when it answers"));
                    store::update_release(st, &r, State::Delivering).await?;
                    store::schedule(st, id, now + 30).await?;
                }
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

/// What committing a release came to.
enum Committed {
    /// Its commit, what it changes of the app's secrets (§3.5), and what it runs that a revert
    /// couldn't undo (§8.1).
    Done(String, Option<String>, Option<String>),
    /// Refused by its preflight (§8.1), why: nothing changed.
    Refused(String),
}

/// Commits a release to its environment's release repository, once its preflight finds nothing
/// against it: the commit, and what it changes of the app's secrets. A commit made before a crash
/// is found by its trailer, not made again.
async fn commit_release(d: &Deploy, r: &Release) -> Result<Committed, String> {
    let a = store::app(&d.hub.store, &r.app).await.map_err(|e| format!("{e:#}"))?.ok_or_else(|| format!("no app {}", r.app))?;
    let env = store::environment(&d.hub.store, &r.environment).await.map_err(|e| format!("{e:#}"))?.ok_or_else(|| format!("no environment {}", r.environment))?;
    let rr = env.release_repo.clone();
    let (hosted, id) = (d.hosted.clone(), r.id);
    let found = {
        let rr = rr.clone();
        crate::rt::blocking(move || hosted.find_trailer(&rr.repo, &rr.branch, repo::RELEASE_TRAILER, &id.to_string())).await.and_then(|x| x).map_err(|e| format!("{e:#}"))?
    };
    if let Some(sha) = found {
        return Ok(Committed::Done(sha, None, None));
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
        Kind::Config | Kind::Revert => prior.as_ref().is_some_and(|p| p.enabled),
        Kind::Deploy | Kind::Rollback | Kind::Enable => true,
    };
    let mut file = repo::release_json(&a, &env.name, r, prior.as_ref(), enabled);
    // The app's secrets, at their versions now (§3.5): what runs gets these until a release pins
    // others, a rollback included.
    let pinned = crate::secrets::of(&d.hub).current(&crate::secrets::Scope::of(&a.name, &env.name)).await.map_err(|e| format!("the app's secrets: {e:#}"))?;
    file.secrets = crate::secrets::versions_text(pinned.iter().map(|(n, v)| (n.as_str(), *v)));
    let changed = secrets_changed(prior.as_ref().map_or("", |p| p.secrets.as_str()), &file.secrets);
    // What it runs, filled in for every provider to read the same (§6): a git path is looked at
    // for a chart where KilnCD can look.
    let chart = match (&a.package, crate::cd::get(), file.version.as_deref()) {
        (Package::Git { .. }, Some(cd), Some(v)) => kilncd::git_chart(&cd, &a.package, v).await,
        _ => false,
    };
    if let Err(why) = repo::resolve(&mut file, &a, chart) {
        return Ok(Committed::Refused(why));
    }
    // Checked before it changes anything: its secrets, its images, and on KilnCD its render and
    // its cluster's dry run (§8.1).
    let reg = store::registration(&d.hub.store, &r.app, &r.environment).await.map_err(|e| format!("{e:#}"))?.ok_or("the app isn't registered there any more")?;
    let found = super::preflight::check(d, &a, &env, &reg, &file, r.values.as_deref()).await;
    if let Some(why) = found.refused {
        return Ok(Committed::Refused(why));
    }
    let mut changes = vec![
        Change { path: repo::app_file(&rr, &a), content: Some(repo::app_yaml(&a).map_err(|e| format!("{e:#}"))?) },
        Change { path: release_path, content: Some(repo::encode(&file).map_err(|e| format!("{e:#}"))?) },
    ];
    if let Some(v) = &r.values {
        changes.push(Change { path: values_path, content: Some(v.clone().into_bytes()) });
    }
    let sha = commit_files(d, &env, changes, repo::message(&a, &env.name, r), &r.requested_by).await?;
    Ok(Committed::Done(sha, Some(changed), found.irreversible))
}

/// What a release changes of the app's secrets, from the versions the one before pinned to its
/// own: each set (`API_KEY v1`) or deleted (`DB_PASSWORD deleted`).
fn secrets_changed(before: &str, after: &str) -> String {
    let read = |t: &str| crate::secrets::parse_versions(t).unwrap_or_default().into_iter().collect::<BTreeMap<String, u32>>();
    let (before, after) = (read(before), read(after));
    let mut out: Vec<String> = after.iter().filter(|(n, v)| before.get(*n) != Some(*v)).map(|(n, v)| format!("{n} v{v}")).collect();
    out.extend(before.keys().filter(|n| !after.contains_key(*n)).map(|n| format!("{n} deleted")));
    out.join(", ")
}

/// What a release's commit holds for its registration.
pub(super) async fn release_file_at(d: &Deploy, a: &App, env: &Environment, commit: &str) -> Result<ReleaseFile, String> {
    let (hosted, rr, path, commit) = (d.hosted.clone(), env.release_repo.clone(), repo::release_file(&env.release_repo, a, &env.name), commit.to_owned());
    let bytes = crate::rt::blocking(move || hosted.read(&rr.repo, &commit, &path)).await.and_then(|x| x).map_err(|e| format!("{e:#}"))?;
    let bytes = bytes.ok_or("the commit holds no release.json for the registration")?;
    repo::decode(&bytes).map_err(|e| format!("release.json doesn't read: {e:#}"))
}

/// What `a` renders in `env` now, and what a release of `version` would render there: what runs,
/// at that version (its values and artifacts as they are), each rendered as KilnCD renders it
/// (docs/deploy-api.md §8.1).
pub(super) async fn kilncd_preview(d: &Deploy, a: &App, env: &Environment, version: &str) -> Result<(Vec<Value>, Vec<Value>), Fail> {
    let cd = crate::cd::get().ok_or_else(|| (StatusCode::SERVICE_UNAVAILABLE, "KilnCD isn't running".to_owned()))?;
    let nothing = || conflict(format!("nothing runs for {} in {} yet: release a version first", a.name, env.name));
    let now = cd.definition(&kilncd::app_id(a, &env.name)).ok_or_else(nothing)?;
    let reg = store::registration(&d.hub.store, &a.name, &env.name).await.map_err(internal)?.ok_or_else(|| missing(format!("{} isn't registered in {}", a.name, env.name)))?;
    let running = match reg.running {
        Some(id) => store::release(&d.hub.store, id).await.map_err(internal)?,
        None => None,
    };
    let commit = running.and_then(|r| r.commit).ok_or_else(nothing)?;
    let mut file = release_file_at(d, a, env, &commit).await.map_err(|e| (StatusCode::BAD_GATEWAY, e))?;
    file.version = Some(version.to_owned());
    // A version that isn't there is said as a release of it would be told.
    if let Some(why) = super::check::missing_version(&d.hub, &a.package, &file).await {
        return Err((StatusCode::UNPROCESSABLE_ENTITY, why));
    }
    let url = d.hosted.url(&env.release_repo.repo).map_err(internal)?;
    let chart = kilncd::git_chart(&cd, &a.package, version).await;
    // Filled again for that version, as a release of it would be.
    file.package = a.package.clone();
    repo::resolve(&mut file, a, chart).map_err(bad)?;
    let then = kilncd::definition(a, env, &reg, &file, &commit, &url, chart).map_err(bad)?;
    futures::try_join!(cd.render_definition(now), cd.render_definition(then)).map_err(|e| (StatusCode::UNPROCESSABLE_ENTITY, format!("{e:#}")))
}

/// A registration's values at a commit (`None`: it has none).
pub(super) async fn values_at(d: &Deploy, a: &App, env: &Environment, commit: &str) -> Result<Option<String>, String> {
    file_at(d, env, commit, repo::values_file(&env.release_repo, a, &env.name)).await
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
        watch_argo(d, name);
        if let (Package::Helm { repo, .. }, Some(_)) = (&file.package, &file.version)
            && let Some(oci) = repo.strip_prefix("oci://")
        {
            argo.ensure_oci_repo(oci.trim_end_matches('/')).await?;
        }
        // Through the environment's ApplicationSet: it's read again now, and makes or updates the
        // release's Application; following the release does the rest, a deletion included.
        if appset::integration_of(&env).is_some() {
            return appset::ensure(d, &argo, &env).await;
        }
        let app_name = argocd::application_name(&a, &env.name);
        if !file.enabled {
            return Ok(argo.delete_application(&app_name, r.keep).await?);
        }
        if file.version.is_none() {
            return Ok(());
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
    let chart = kilncd::git_chart(&cd, &a.package, file.version.as_deref().unwrap_or_default()).await;
    let def = kilncd::definition(&a, &env, &reg, &file, &commit, &url, chart).map_err(Undelivered::Refused)?;
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
        watch_argo(d, name);
        if appset::integration_of(&env).is_some() {
            let reg = store::registration(&d.hub.store, &r.app, &r.environment).await.map_err(|e| format!("{e:#}"))?.ok_or("the app isn't registered there any more")?;
            return appset::follow(d, &argo, r, &a, &env, &reg, &file, commit).await;
        }
        let app_name = argocd::application_name(&a, &env.name);
        // What its stream last said, or Argo CD asked.
        let current = match d.argo.application(name, &app_name) {
            Some(app) => Some(app),
            None => argo.application(&app_name, None).await?,
        };
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
        // written again. Its commit says whose it is: a revert's (or a commit kiln didn't make)
        // names in its release.json the release it puts back, not itself.
        let stamped = current.as_ref().and_then(|c| c.pointer("/metadata/annotations/kiln.dev~1commit").and_then(Value::as_str).map(str::to_owned));
        if stamped.as_deref() != Some(commit) {
            return match deliver(d, r).await {
                Ok(()) => Ok((Landed::No, json!({ "application": app_name, "message": "written again" }))),
                Err(Undelivered::Refused(why)) => Ok((Landed::Refused(why), json!({ "application": app_name }))),
                Err(Undelivered::Unreachable(e)) => Err(e),
            };
        }
        let current = current.unwrap_or(Value::Null);
        // Delivered as its release.json says: a revert names the release it puts back.
        return Ok(argocd::landed(&current, file.release, &version));
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
        // What runs is a release that landed: nothing here needs a person any more.
        reg.degraded = None;
        match r.kind {
            Kind::Disable => reg.enabled = false,
            Kind::Deploy | Kind::Rollback | Kind::Enable => reg.enabled = true,
            // A revert puts back what ran, enabled as it was.
            Kind::Config | Kind::Revert => {}
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
    if state == State::Succeeded && r.kind == Kind::Deploy {
        promote_onward(d, &r).await;
    }
    // A release waiting behind this one may go now.
    d.wake.notify();
    Ok(())
}

/// The environments a version that landed in `from` moves on to by itself: those that promote
/// automatically with `from` among their `after`, where the app is registered.
fn onward(envs: &[Environment], registered: &[String], from: &str) -> Vec<String> {
    envs.iter()
        .filter(|e| e.gates.promote == super::model::Promote::Auto && e.gates.after.iter().any(|a| a == from) && registered.contains(&e.name))
        .map(|e| e.name.clone())
        .collect()
}

/// A version that landed, released on by itself (docs/deploy-api.md §9.2): exactly what landed,
/// to each environment that promotes automatically after this one, through its other gates
/// (soak, approvals, windows, freezes). Asked for in the name of who asked for the landed one, so
/// they don't approve their own change on its way; once per landing and environment however often
/// this runs. One refused (a release already in flight there, an `after` it doesn't run in yet)
/// goes when the next of its `after` lands, or by hand.
async fn promote_onward(d: &Deploy, landed: &Release) {
    let st = &d.hub.store;
    let next = async {
        let envs = store::environments(st).await?;
        let registered: Vec<String> = store::registrations(st, Some(&landed.app), None).await?.into_iter().map(|r| r.environment).collect();
        anyhow::Ok(onward(&envs, &registered, &landed.environment))
    };
    let next = match next.await {
        Ok(n) => n,
        Err(e) => {
            tracing::warn!(app = %landed.app, error = %format!("{e:#}"), "deploy: the environments a version promotes to by itself didn't read");
            return;
        }
    };
    let who = Who { name: landed.requested_by.clone(), agent: false };
    for env in next {
        // A registration that needs a person is theirs to release to.
        if let Ok(Some(reg)) = store::registration(st, &landed.app, &env).await
            && let Some(why) = reg.degraded
        {
            tracing::info!(app = %landed.app, from = %landed.environment, to = %env, %why, "deploy: a version doesn't promote by itself where a person is needed");
            continue;
        }
        let ask = Ask { app: landed.app.clone(), environment: env.clone(), promote_from: Some(landed.environment.clone()), idempotency_key: Some(format!("promote:{}:{env}", landed.id)), ..Ask::default() };
        match Box::pin(release(d, &who, ask)).await {
            Ok(r) => tracing::info!(app = %landed.app, from = %landed.environment, to = %env, release = r.id, "deploy: a version promoted by itself"),
            Err((_, why)) => tracing::info!(app = %landed.app, from = %landed.environment, to = %env, %why, "deploy: a version didn't promote by itself"),
        }
    }
}

/// The trailer of a commit that reverts a release's: the release.
const REVERT_TRAILER: &str = "Kiln-Reverts";

/// What a release that couldn't land leaves in git: kiln's own commit is undone; a commit kiln
/// didn't make is someone's, and stays.
/// What's Degraded of a release's registration, object by object as its provider says it: the
/// evidence it failed on (`Deployment web: ReplicaSet "web-6f9d" has timed out progressing`).
async fn degraded_objects(d: &Deploy, r: &Release) -> Option<String> {
    let a = store::app(&d.hub.store, &r.app).await.ok()??;
    let env = store::environment(&d.hub.store, &r.environment).await.ok()??;
    let read = super::reads::read(d, &a, &env, super::reads::Read::Resources).await.ok()?;
    let text = |o: &Value, k: &str| o.get(k).and_then(Value::as_str).filter(|v| !v.is_empty()).map(str::to_owned);
    let objects: Vec<String> = read
        .get("resources")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter(|o| o.get("health").and_then(Value::as_str) == Some("Degraded"))
        .map(|o| {
            let said = text(o, "message").or_else(|| text(o, "why")).unwrap_or_else(|| "Degraded".to_owned());
            format!("{} {}: {said}", text(o, "kind").unwrap_or_default(), text(o, "name").unwrap_or_default())
        })
        .collect();
    (!objects.is_empty()).then(|| objects.join("; "))
}

/// What follows a release that failed after its commit (§8.1).
#[derive(Debug, PartialEq, Eq)]
pub(super) enum Aftermath {
    /// What ran before is put back: its commit reverted, as a release followed until it lands.
    Revert,
    /// It stays as it is, for a person: why. The registration says it needs one.
    Hold(String),
}

/// What follows a release that failed after its commit. A rollback isn't reverted (that would bring
/// back what it rolled back from), nor a revert (kiln doesn't revert its own way back), nor a commit
/// kiln didn't make (git is its author's), nor one asked not to be.
pub(super) fn aftermath(r: &Release) -> Aftermath {
    match (r.origin, r.kind) {
        (Origin::Git, _) => Aftermath::Hold("it is a commit kiln didn't make: git is its author's".into()),
        (_, Kind::Revert) => Aftermath::Hold(format!("putting back what ran before release {} didn't land either", r.reverts.unwrap_or_default())),
        (_, Kind::Rollback) => Aftermath::Hold("it is a rollback: what it rolled back from isn't brought back".into()),
        _ if r.no_revert => Aftermath::Hold("it was asked not to be reverted".into()),
        _ if r.irreversible.is_some() => Aftermath::Hold(format!("it runs {} on its way in, which a revert couldn't undo", r.irreversible.as_deref().unwrap_or_default())),
        _ => Aftermath::Revert,
    }
}

/// A release that failed after its commit, for `why`: what ran before put back (a revert, followed
/// until it lands), or the release held as it is for a person.
async fn failed(d: &Deploy, mut r: Release, why: String) -> anyhow::Result<()> {
    if let Aftermath::Hold(reason) = aftermath(&r) {
        return hold(d, r, &why, &reason).await;
    }
    match revert(d, &r, &why).await {
        Ok((Some(sha), _)) => match start_revert(d, &r, &sha).await {
            Ok(back) => {
                r.reverted_by = Some(back.id);
                finish(d, r, State::Failed, Some(format!("{why}; release {} puts back what ran before", back.id))).await
            }
            Err(e) => hold(d, r, &why, &format!("its commit is reverted in {}, but putting that through failed: {e:#}", short(&sha))).await,
        },
        // Someone changed its files since: what runs is theirs.
        Ok((None, _)) => finish(d, r, State::Failed, Some(format!("{why}; nothing of its commit was left to revert"))).await,
        Err(e) => hold(d, r, &why, &format!("its commit couldn't be reverted ({e})")).await,
    }
}

/// A release its provider refused, or never got: nothing of it was applied, and git goes back
/// with it. A revert refused isn't reverted, and a rollback refused leaves running what it rolled
/// back from: both need a person.
async fn refused(d: &Deploy, r: Release, why: String) -> anyhow::Result<()> {
    if r.kind == Kind::Revert {
        return hold(d, r, &why, "putting back what ran before was refused too").await;
    }
    let undone = give_back(d, &r, &why).await;
    let message = format!("{why}; {undone}");
    if r.kind == Kind::Rollback {
        degrade(d, &r, &format!("release {} failed: {message}; what it rolled back from runs on", r.id)).await?;
    }
    finish(d, r, State::Failed, Some(message)).await
}

/// Fails a release held as it is: the registration says it needs a person, and why.
async fn hold(d: &Deploy, r: Release, why: &str, reason: &str) -> anyhow::Result<()> {
    let message = format!("{why}; held: {reason}");
    degrade(d, &r, &format!("release {} failed: {message}", r.id)).await?;
    finish(d, r, State::Failed, Some(message)).await
}

/// Marks a release's registration as needing a person, for `why`.
async fn degrade(d: &Deploy, r: &Release, why: &str) -> anyhow::Result<()> {
    let st = &d.hub.store;
    if let Some(mut reg) = store::registration(st, &r.app, &r.environment).await? {
        reg.degraded = Some(why.to_owned());
        store::put_registration(st, &reg).await?;
    }
    Ok(())
}

/// The release that puts back what ran before `failed`, whose commit `sha` reverted: delivered and
/// followed as any release, so it says whether what ran before runs again. Asked for once however
/// often this runs (a restart between the revert and its record).
async fn start_revert(d: &Deploy, failed: &Release, sha: &str) -> anyhow::Result<Release> {
    let st = &d.hub.store;
    let a = store::app(st, &failed.app).await?.ok_or_else(|| anyhow::anyhow!("no app {}", failed.app))?;
    let env = store::environment(st, &failed.environment).await?.ok_or_else(|| anyhow::anyhow!("no environment {}", failed.environment))?;
    let running = store::registration(st, &failed.app, &failed.environment).await?.and_then(|reg| reg.running);
    let file = release_file_at(d, &a, &env, sha).await.ok();
    let now = crate::util::now_unix();
    let back = Release {
        id: 0,
        app: failed.app.clone(),
        environment: failed.environment.clone(),
        kind: Kind::Revert,
        state: State::Delivering,
        origin: Origin::Kiln,
        version: file.as_ref().and_then(|f| f.version.clone()),
        artifacts: file.map(|f| f.artifacts).unwrap_or_default(),
        values: None,
        commit: Some(sha.to_owned()),
        requested_by: "kiln".to_owned(),
        message: Some(format!("putting back what ran before release {}", failed.id)),
        rollback_of: running,
        rolled_back_by: None,
        keep: true,
        provider: Value::Null,
        gates: Vec::new(),
        approvals: Vec::new(),
        promoted_from: None,
        idempotency_key: Some(format!("revert:{}", failed.id)),
        created_at: now,
        committed_at: Some(now),
        ended_at: None,
        secrets: None,
        no_revert: false,
        reverts: Some(failed.id),
        reverted_by: None,
        irreversible: None,
    };
    let (back, new) = store::insert_release(st, &back).await?;
    if new {
        // Not handed over now, it is when it's followed: `follow` delivers what isn't there.
        if let Err(e) = deliver(d, &back).await {
            tracing::warn!(release = back.id, error = %e, "deploy: handing a revert to its provider failed; following it hands it again");
        }
        store::schedule(st, back.id, now + 2).await?;
        audit(d, "kiln", "deploy.release", format!("revert of release {} in {}: release {}", failed.id, back.environment, back.id)).await;
        emit_release(d, &back);
    }
    Ok(back)
}

async fn give_back(d: &Deploy, r: &Release, why: &str) -> String {
    if r.origin == Origin::Git {
        return "it is a commit kiln didn't make, so git keeps it".to_owned();
    }
    undo(d, r, why).await
}

/// Puts git and the provider back after a release that won't land (failed, refused, or cancelled
/// while delivering): its commit is reverted, and the provider handed what git says then. Nothing is
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
                    secrets: None,
                    no_revert: false,
                    reverts: None,
                    reverted_by: None,
                    irreversible: None,
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
            argocd: Default::default(),
        }
    }

    #[test]
    fn a_template_s_package_becomes_the_app_s_and_backstage_reads_the_app() -> Result<(), String> {
        let p = json!({ "kind": "helm", "repo": "oci://ghcr.io/acme/charts", "chart": "{app}", "set": { "image.repository": "ghcr.io/acme/{app}" } });
        let filled = fill_app(p, "web");
        assert_eq!(filled.pointer("/chart"), Some(&json!("web")));
        assert_eq!(filled.pointer("/set/image.repository"), Some(&json!("ghcr.io/acme/web")));
        let package: Package = serde_json::from_value(filled).map_err(|e| e.to_string())?;
        let a = App {
            name: "web".into(),
            project: "shop".into(),
            owners: vec!["payments".into(), "platform".into()],
            package,
            namespace: Some("apps-web".into()),
            properties: BTreeMap::new(),
            template: Some("service".into()),
            contacts: Contacts { oncall: Some("ada".into()), channels: vec!["#deploys".into()], links: BTreeMap::from([("dashboard".into(), "https://grafana.example/d/web".into())]) },
            source: Some("shop/web".into()),
            archived: false,
        };
        let doc: Value = serde_yaml::from_str(&catalog_info(&a)).map_err(|e| e.to_string())?;
        assert_eq!(doc.pointer("/metadata/annotations/kiln.dev~1source"), Some(&json!("shop/web")));
        assert_eq!(doc.pointer("/kind"), Some(&json!("Component")));
        assert_eq!(doc.pointer("/spec/owner"), Some(&json!("payments")), "the owner team first");
        assert_eq!(doc.pointer("/spec/type"), Some(&json!("service")));
        assert_eq!(doc.pointer("/metadata/links/0/url"), Some(&json!("https://grafana.example/d/web")));
        assert_eq!(doc.pointer("/metadata/annotations/kiln.dev~1oncall"), Some(&json!("ada")));
        Ok(())
    }

    #[test]
    fn a_fleet_change_sets_values_in_each_apps_own() -> Result<(), String> {
        let set = BTreeMap::from([("replicaCount".to_owned(), "2".to_owned()), ("image.pullPolicy".to_owned(), "Always".to_owned()), ("ui.enabled".to_owned(), "true".to_owned())]);
        let merged = merge_values("# the app's own\nimage:\n  repository: ghcr.io/acme/web\nui:\n  message: hi\n", &set)?;
        let v: serde_yaml::Value = serde_yaml::from_str(&merged).map_err(|e| e.to_string())?;
        assert_eq!(v.get("replicaCount"), Some(&serde_yaml::Value::Number(2.into())), "a number stays a number");
        assert_eq!(v.get("image").and_then(|i| i.get("repository")).and_then(serde_yaml::Value::as_str), Some("ghcr.io/acme/web"), "what it had stays");
        assert_eq!(v.get("image").and_then(|i| i.get("pullPolicy")).and_then(serde_yaml::Value::as_str), Some("Always"));
        assert_eq!(v.get("ui").and_then(|u| u.get("enabled")), Some(&serde_yaml::Value::Bool(true)));
        assert_eq!(v.get("ui").and_then(|u| u.get("message")).and_then(serde_yaml::Value::as_str), Some("hi"));
        // From nothing, and where a path runs into something that isn't a map.
        let fresh = merge_values("", &BTreeMap::from([("a.b".to_owned(), "x".to_owned())]))?;
        assert_eq!(fresh.trim(), "a:\n  b: x");
        assert!(merge_values("image: web\n", &BTreeMap::from([("image.tag".to_owned(), "1".to_owned())])).is_err_and(|e| e.contains("image isn't a map")));
        assert!(merge_values("", &BTreeMap::from([("a..b".to_owned(), "1".to_owned())])).is_err());
        Ok(())
    }

    #[test]
    fn a_version_moves_on_by_itself_only_where_an_environment_says() {
        let after = |name: &str, upstream: &[&str], auto: bool| {
            let mut e = env(false);
            e.name = name.into();
            e.gates.after = upstream.iter().map(|u| (*u).to_owned()).collect();
            e.gates.promote = if auto { super::super::model::Promote::Auto } else { super::super::model::Promote::Manual };
            e
        };
        let envs = vec![after("staging", &[], false), after("production", &["staging"], true), after("eu", &["staging"], false), after("canary", &["staging"], true)];
        let registered = vec!["staging".to_owned(), "production".to_owned(), "eu".to_owned()];
        assert_eq!(onward(&envs, &registered, "staging"), ["production"], "eu promotes by hand; canary doesn't run the app");
        assert!(onward(&envs, &registered, "production").is_empty(), "nothing comes after production");
        assert_eq!(serde_json::to_value(super::super::model::Gates::default()).ok(), Some(json!({})), "`manual`, the default, isn't written");
    }

    fn failed_release(kind: Kind, origin: Origin) -> Release {
        Release {
            id: 42,
            app: "web".into(),
            environment: "production".into(),
            kind,
            state: State::Delivering,
            origin,
            version: Some("1.5.0".into()),
            artifacts: BTreeMap::new(),
            values: None,
            commit: Some("abc".into()),
            requested_by: "ada".into(),
            message: None,
            rollback_of: None,
            rolled_back_by: None,
            keep: false,
            provider: Value::Null,
            gates: Vec::new(),
            approvals: Vec::new(),
            promoted_from: None,
            idempotency_key: None,
            created_at: 0,
            committed_at: Some(0),
            ended_at: None,
            secrets: None,
            no_revert: false,
            reverts: None,
            reverted_by: None,
            irreversible: None,
        }
    }

    /// §8.1: what ran before comes back after a release that failed, unless that would bring back
    /// what someone chose to leave, undo kiln's own way back, or overrule git or the person who
    /// asked: those are held for a person.
    #[test]
    fn a_failed_release_is_reverted_unless_that_would_make_things_worse() {
        let held = |r: &Release| matches!(aftermath(r), Aftermath::Hold(_));
        assert_eq!(aftermath(&failed_release(Kind::Deploy, Origin::Api)), Aftermath::Revert);
        assert_eq!(aftermath(&failed_release(Kind::Config, Origin::Api)), Aftermath::Revert, "a secrets or values release too");
        assert_eq!(aftermath(&failed_release(Kind::Enable, Origin::Api)), Aftermath::Revert);
        assert!(held(&failed_release(Kind::Rollback, Origin::Api)), "a rollback doesn't bring back what it rolled back from");
        assert!(held(&failed_release(Kind::Revert, Origin::Kiln)), "kiln doesn't revert its own way back");
        assert!(held(&failed_release(Kind::Deploy, Origin::Git)), "git is its author's");
        let asked = Release { no_revert: true, ..failed_release(Kind::Config, Origin::Api) };
        assert!(matches!(aftermath(&asked), Aftermath::Hold(why) if why.contains("asked not to be reverted")));
        let migrates = Release { irreversible: Some("Job web-migrate".into()), ..failed_release(Kind::Deploy, Origin::Api) };
        assert!(matches!(aftermath(&migrates), Aftermath::Hold(why) if why.contains("Job web-migrate")), "a migration that ran isn't undone by putting the code back");
    }

    #[test]
    fn a_release_says_what_it_changes_of_the_secrets() {
        assert_eq!(secrets_changed("", "API_KEY@1,DB_PASSWORD@2"), "API_KEY v1, DB_PASSWORD v2");
        assert_eq!(secrets_changed("API_KEY@1,DB_PASSWORD@2", "API_KEY@1,DB_PASSWORD@3,TOKEN@1"), "DB_PASSWORD v3, TOKEN v1");
        assert_eq!(secrets_changed("API_KEY@1,DB_PASSWORD@2", "API_KEY@1"), "DB_PASSWORD deleted");
        assert_eq!(secrets_changed("API_KEY@1", "API_KEY@1"), "", "nothing changed");
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
