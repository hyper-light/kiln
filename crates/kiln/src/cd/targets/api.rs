//! The providers' operations (docs/cd-targets.md §9), for the builder and import: the catalog,
//! the targets in use, what an account holds, and a definition checked and rendered before it is
//! saved.

use std::collections::BTreeMap;

use http::StatusCode;
use kiln_cd::apps::{self, Engine, EnvironmentSpec};
use kiln_cd::git::Mirror;
use kiln_cd::providers::{self, CATALOG, Target};
use serde_json::{Value, json};

use super::{Conn, Cx as TargetCx};
use crate::cd::ops::{Cx, Out};
use crate::ci::integrations;

fn bad(m: impl Into<String>) -> (StatusCode, String) {
    (StatusCode::BAD_REQUEST, m.into())
}

fn s<'a>(v: &'a Value, k: &str) -> Option<&'a str> {
    v.get(k).and_then(Value::as_str).map(str::trim).filter(|s| !s.is_empty())
}

/// `cd.providers.list`: every provider, what it takes, and the accounts that could serve it.
pub async fn providers(cx: Cx, _: Value) -> Out {
    let all = integrations::list(&cx.hub).await.map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, format!("{e:#}")))?;
    let out: Vec<Value> = CATALOG
        .iter()
        .map(|p| {
            let accounts: Vec<Value> = all.iter().filter(|i| i.kind == p.integration).map(|i| json!({ "name": i.name, "verified_at": i.verified_at, "last_error": i.last_error })).collect();
            let mut v = serde_json::to_value(p).unwrap_or(Value::Null);
            if let Some(m) = v.as_object_mut() {
                m.insert("accounts".into(), Value::Array(accounts));
                m.insert("integration_title".into(), json!(integrations::kind(p.integration).map(|k| k.title)));
            }
            v
        })
        .collect();
    Ok(json!({ "providers": out }))
}

/// `cd.targets.list`: the targets apps deploy to, each with its state and its apps.
pub async fn targets(cx: Cx, _: Value) -> Out {
    let mut by: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for (id, e) in cx.cd.apps.read().iter() {
        if providers::is_target(&e.def.destination.cluster) {
            by.entry(e.def.destination.cluster.clone()).or_default().push(id.clone());
        }
    }
    let errors = cx.cd.target_errors.lock().clone();
    let out: Vec<Value> = by
        .into_iter()
        .filter_map(|(name, apps)| {
            let t = Target::parse(&name)?;
            let def = t.def()?;
            let place: serde_json::Map<String, Value> = def.place.iter().zip(&t.place).map(|(k, v)| ((*k).to_owned(), json!(v))).collect();
            Some(json!({
                "name": name, "provider": t.provider, "title": def.title, "account": t.account, "place": place,
                "connected": cx.cd.target(&name).is_some(), "error": errors.get(&name), "apps": apps,
            }))
        })
        .collect();
    Ok(json!({ "targets": out }))
}

/// A target from a request: `provider`, `account`, and the provider's place fields.
fn target_of(v: &Value) -> Result<Conn, (StatusCode, String)> {
    let provider = s(v, "provider").ok_or_else(|| bad("`provider` is required"))?;
    let mut spec = serde_json::Map::new();
    spec.insert("account".into(), json!(s(v, "account").ok_or_else(|| bad("`account` is required: the integration it acts through"))?));
    let def = providers::get(provider).ok_or_else(|| bad(format!("no provider {provider}")))?;
    for k in def.place {
        if let Some(x) = s(v, k) {
            spec.insert((*k).to_owned(), json!(x));
        }
    }
    let (t, _) = providers::target_of(provider, &spec).map_err(bad)?;
    if !def.built {
        return Err(bad(format!("{} isn't built yet", def.title)));
    }
    Ok(Conn::new(t, def))
}

/// `cd.discover`: what an account holds there, for the builder and import (§8.2, §8.3). With
/// `name`, that one resource and the definition that adopts it as it runs now.
pub async fn discover(cx: Cx, v: Value) -> Out {
    let conn = target_of(&v)?;
    let creds = cx.cd.creds(&conn, None, "discover").await.map_err(|e| (StatusCode::BAD_GATEWAY, format!("{e:#}")))?;
    let tcx = TargetCx { conn: &conn, creds, records: cx.cd.as_ref() };
    let scope = s(&v, "scope").map(str::to_owned);
    let r = match (conn.def.name, s(&v, "name")) {
        ("aws-lambda", None) => super::aws_lambda::discover(&tcx).await,
        ("aws-lambda", Some(n)) => super::aws_lambda::adopt(&tcx, n).await,
        ("aws-ecs", None) => super::aws_ecs::discover(&tcx, scope.as_deref()).await,
        ("aws-ecs", Some(n)) => super::aws_ecs::adopt(&tcx, scope.as_deref().unwrap_or_default(), n).await,
        (other, _) => return Err(bad(format!("no discovery for {other} yet"))),
    };
    r.map_err(|e| (StatusCode::BAD_GATEWAY, format!("{e:#}")))
}

/// `cd.validate`: a deploy file (and environments) checked as kiln reads them, each app with
/// where it goes and, for a provider's, the objects it renders now: from `repo`'s files at its
/// trunk, with its artifacts as they resolve now.
pub async fn validate(cx: Cx, v: Value) -> Out {
    let text = s(&v, "deploy").ok_or_else(|| bad("`deploy` is required: the deploy file's text"))?;
    let envs: BTreeMap<String, EnvironmentSpec> = match s(&v, "environments") {
        Some(e) => serde_yaml::from_str(e).map_err(|e| bad(format!(".kiln/environments.yaml: {e}")))?,
        None => BTreeMap::new(),
    };
    let repos = cx.hub.store.repos().await.map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, format!("{e:#}")))?;
    let repo = match s(&v, "repo") {
        Some(name) => Some(repos.iter().find(|r| r.name == name || r.clone_url == name).ok_or_else(|| bad(format!("{name} isn't one of kiln's repositories")))?),
        None => None,
    };
    let (url, trunk) = match repo {
        Some(r) => (r.clone_url.clone(), r.branches.first().cloned().unwrap_or_else(|| "main".into())),
        None => (String::new(), "main".to_owned()),
    };
    // The repository at trunk, for what renders read.
    let (dir, auth) = (cx.cd.mirror_dir(&url), cx.cd.git_auth(&url));
    let (u, t) = (url.clone(), trunk.clone());
    let at = if url.is_empty() {
        None
    } else {
        crate::rt::blocking(move || -> anyhow::Result<(Mirror, String)> {
            let m = Mirror::open(&dir, &u, auth)?;
            m.fetch()?;
            let c = m.resolve(&t)?;
            Ok((m, c))
        })
        .await
        .ok()
        .and_then(Result::ok)
    };
    let mut scope = apps::Scope::default();
    if let Some((_, c)) = &at {
        scope.set("git.sha", json!(c));
        scope.set("git.short_sha", json!(c.get(..7).unwrap_or(c)));
    }
    let defs = match apps::from_deploy_file(text, &url, &trunk, &envs, &scope) {
        Ok(d) => d,
        Err(e) => return Ok(json!({ "ok": false, "error": e.0, "apps": [] })),
    };
    let mut out = Vec::new();
    for def in defs {
        let mut app = json!({ "id": def.id(), "name": def.name, "environment": def.environment, "destination": { "cluster": def.destination.cluster, "namespace": def.destination.namespace } });
        let source = def.sources.first();
        if let Some(Engine::Provider(p)) = source.map(|s| &s.engine) {
            if let Some(m) = app.as_object_mut() {
                m.insert("provider".into(), json!(p.provider));
            }
            let rendered = match &at {
                None => Err("name a repository (`repo`) to render with its files".to_owned()),
                Some((mirror, commit)) => render_provider(&cx, &def, mirror, commit).await,
            };
            if let Some(m) = app.as_object_mut() {
                match rendered {
                    Ok(objs) => m.insert("objects".into(), Value::Array(objs)),
                    Err(e) => m.insert("error".into(), json!(e)),
                };
            }
        }
        out.push(app);
    }
    Ok(json!({ "ok": true, "apps": out }))
}

/// A provider app's objects at `commit`, its artifacts resolved as a revision's would be.
async fn render_provider(cx: &Cx, def: &apps::AppDef, mirror: &Mirror, commit: &str) -> Result<Vec<Value>, String> {
    let (refs, _) = cx.cd.resolve_artifacts(def, commit).await.map_err(|e| format!("{e:#}"))?;
    let sources = apps::with_artifacts(&def.sources, &refs).map_err(|e| e.0)?;
    let source = sources.first().ok_or("no source")?;
    let Engine::Provider(p) = &source.engine else { return Err("not a provider's app".into()) };
    let mut scope = apps::Scope::default();
    scope.set("git.sha", json!(commit));
    scope.set("git.short_sha", json!(commit.get(..7).unwrap_or(commit)));
    let spec = scope.interpolate_value_keeping(&p.spec, &["artifacts"]);
    let read = |rel: &str| -> Option<Vec<u8>> { mirror.read(commit, &kiln_cd::render::join(&source.path, rel)?).ok().flatten() };
    let input = providers::RenderInput { spec: &spec, target: &p.target, app: &def.name, namespace: &def.destination.namespace, revision: commit, read: &read };
    providers::render(&p.provider, &input)
}

/// `cd.apps.keep`: the apps a deploy file defines, kept in kiln rather than in the repository
/// (§8.2 step 7, "Keep in kiln"). They read the repository's files as any app does; they are
/// marked as kiln's own.
pub async fn keep(cx: Cx, v: Value) -> Out {
    let text = s(&v, "deploy").ok_or_else(|| bad("`deploy` is required: the deploy file's text"))?;
    let envs: BTreeMap<String, EnvironmentSpec> = match s(&v, "environments") {
        Some(e) => serde_yaml::from_str(e).map_err(|e| bad(format!(".kiln/environments.yaml: {e}")))?,
        None => BTreeMap::new(),
    };
    let repo_name = s(&v, "repo").ok_or_else(|| bad("`repo` is required: the repository its files come from"))?;
    let repos = cx.hub.store.repos().await.map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, format!("{e:#}")))?;
    let repo = repos.iter().find(|r| r.name == repo_name || r.clone_url == repo_name).ok_or_else(|| bad(format!("{repo_name} isn't one of kiln's repositories")))?;
    let trunk = repo.branches.first().cloned().unwrap_or_else(|| "main".into());
    if cx.actor.starts_with("agent:") && !cx.hub.cfg.cd.agents_deploy {
        return Err((StatusCode::FORBIDDEN, "agents may not create apps: ask a person to, or have them allow agents to deploy ([cd] agents_deploy = true)".into()));
    }
    let defs = apps::from_deploy_file(text, &repo.clone_url, &trunk, &envs, &apps::Scope::default()).map_err(|e| bad(e.0))?;
    let mut kept = Vec::new();
    for mut def in defs {
        def.origin = apps::Origin::Api;
        def.labels.insert("kiln.dev/kept".into(), "true".into());
        let id = def.id();
        match cx.cd.store.submit(kiln_cd::state::Command::PutApp(Box::new(def))).await {
            Ok(Ok(())) => kept.push(id),
            Ok(Err(e)) => return Err(bad(e)),
            Err(_) => return Err((StatusCode::SERVICE_UNAVAILABLE, "the store stopped".into())),
        }
    }
    cx.hub.store.audit(cx.actor.clone(), "cd.apps.keep".into(), kept.join(",")).await;
    Ok(json!({ "apps": kept }))
}
