//! KilnCD as a CD provider (docs/deploy-api.md §8.2). A registration is one KilnCD app,
//! `<project>/<app>-<env>`, made from the release commit: the app's package at the release's
//! version, and the release repository at that very commit (`$release/…/values.yaml`). KilnCD
//! syncs it automatically, since the gates ran before the commit; the release has landed when
//! KilnCD's history shows a successful sync of that commit and the app is synced and healthy.

use std::collections::BTreeMap;
use std::sync::Arc;

use kiln_cd::apps::{self, AppDef, Destination, Engine, Helm, HelmParam, Origin, ProviderSource, Source};
use kiln_cd::model::{SyncOptions, SyncPolicy, Tracking};
use serde_json::{Value, json};

use super::model::{App, Environment, Package, Registration, ReleaseFile};
use super::provider::{Landed, Undelivered};
use super::repo;
use crate::cd::Cd;
use crate::hub::Hub;

/// KilnCD's id for a registration's app.
pub fn app_id(app: &App, env: &str) -> String {
    format!("{}/{}-{env}", app.project, app.name)
}

/// The definition KilnCD deploys a release from.
pub fn definition(app: &App, env: &Environment, reg: &Registration, file: &ReleaseFile, commit: &str, release_url: &str) -> Result<AppDef, String> {
    let version = file.version.clone().ok_or("the release names no version")?;
    let values = format!("$release/{}", repo::values_file(&env.release_repo, app, &env.name));
    let set = repo::filled_set(&app.package, file)?;
    let raw_set = app.package.set();
    // What comes from the release (a version, a tag, a digest) stays a string, whatever it looks
    // like: Helm reads `--set image.tag=1.10` as the number 1.1.
    let parameters: Vec<HelmParam> = set
        .into_iter()
        .map(|(name, value)| {
            let force_string = raw_set.get(&name).is_some_and(|t| t.contains("${{"));
            HelmParam { name, value, force_string }
        })
        .collect();
    let helm = |value_files: Vec<String>| Helm {
        value_files,
        parameters: parameters.clone(),
        release_name: Some(app.name.clone()),
        ignore_missing_value_files: true,
        ..Helm::default()
    };
    let kubernetes = Destination { cluster: env.targets.kubernetes.first().cloned().unwrap_or_else(|| apps::IN_CLUSTER.to_owned()), namespace: app.namespace().to_owned() };
    let (package, destination) = match &app.package {
        Package::Platform { provider, spec, repo: files, path } => {
            let place = env.targets.platforms.get(provider).ok_or_else(|| format!("{} has no {provider} target: give it targets.platforms.{provider}: {{ account: <integration>, … }}", env.name))?;
            let (target, extra) = kiln_cd::providers::target_of(provider, place).map_err(|e| format!("{}: {e}", env.name))?;
            let spec = repo::fill_value(spec, file, repo::PLATFORM_KEEPS)?;
            // A provider's scope (an ECS cluster) is its definition's: the app's namespace.
            let scope = kiln_cd::providers::get(provider).and_then(|d| d.scope).and_then(|k| spec.get(k)).and_then(Value::as_str).unwrap_or_default().to_owned();
            // Its files come from its repository at the release's version; without one, from the
            // registration's directory of the release repository, at the release's commit.
            let (repo, path, revision) = match files {
                Some(r) => (r.clone(), path.clone(), version.clone()),
                None => (release_url.to_owned(), repo::registration_dir(&env.release_repo, app, &env.name), commit.to_owned()),
            };
            let engine = Engine::Provider(ProviderSource { provider: provider.clone(), spec, target: extra });
            (Source { repo, path, revision, chart: None, as_ref: None, engine }, Destination { cluster: target.name(), namespace: scope })
        }
        Package::Helm { repo, chart, chart_version, .. } => (Source {
            repo: repo.clone(),
            path: String::new(),
            revision: match chart_version {
                Some(v) => repo::fill(v, file)?,
                None => version,
            },
            chart: Some(chart.clone()),
            as_ref: None,
            engine: Engine::Helm(helm(vec![values])),
        }, kubernetes),
        Package::Git { repo, path, .. } => (Source {
            repo: repo.clone(),
            path: path.clone(),
            revision: version,
            chart: None,
            as_ref: None,
            engine: if parameters.is_empty() { Engine::Auto } else { Engine::Helm(helm(vec![values])) },
        }, kubernetes),
    };
    let platform = matches!(app.package, Package::Platform { .. });
    let release = Source { repo: release_url.to_owned(), path: String::new(), revision: commit.to_owned(), chart: None, as_ref: Some("release".into()), engine: Engine::Auto };
    // A namespace is Kubernetes'.
    let (options, _) = SyncOptions::parse(if platform { Vec::new() } else { vec!["CreateNamespace=true"] });
    Ok(AppDef {
        name: format!("{}-{}", app.name, env.name),
        project: app.project.clone(),
        sources: vec![package, release],
        destination,
        sync: SyncPolicy { auto: !reg.paused, prune: env.sync.prune, self_heal: env.sync.self_heal, allow_empty: false, options, retry: None },
        ignore: Vec::new(),
        tracking: Tracking::Kiln,
        origin: Origin::Api,
        environment: None,
        labels: BTreeMap::from([("kiln.dev/registration".to_owned(), format!("{}/{}", app.name, env.name)), ("kiln.dev/release".to_owned(), file.release.to_string())]),
        history_limit: 20,
        cascade: true,
        managed_namespace: None,
        artifacts: BTreeMap::new(),
        rollout: None,
        checks: BTreeMap::new(),
    })
}

fn cx(hub: &Arc<Hub>, cd: &Arc<Cd>, actor: &str) -> crate::cd::ops::Cx {
    crate::cd::ops::Cx { hub: Arc::clone(hub), cd: Arc::clone(cd), actor: actor.to_owned() }
}

async fn op(hub: &Arc<Hub>, cd: &Arc<Cd>, actor: &str, name: &str, input: Value) -> Result<Value, String> {
    crate::cd::ops::call(cx(hub, cd, actor), name, input).await.map_err(|(code, msg)| format!("KilnCD {name}: {msg} ({code})"))
}

/// Puts a release's definition in place and has KilnCD look at it now. A definition KilnCD won't
/// take is refused.
pub async fn deliver(hub: &Arc<Hub>, cd: &Arc<Cd>, actor: &str, def: &AppDef) -> Result<(), Undelivered> {
    let id = def.id();
    crate::cd::ops::call(cx(hub, cd, actor), "cd.apps.create", json!({ "definition": def }))
        .await
        .map_err(|(code, msg)| Undelivered::of_status(code.as_u16(), format!("KilnCD cd.apps.create: {msg} ({code})")))?;
    // Registered just now, KilnCD may not hold it yet: a refresh that misses is no failure.
    let _ = op(hub, cd, actor, "cd.apps.refresh", json!({ "app": id, "hard": true })).await;
    Ok(())
}

/// Removes a registration's app: its objects too, unless `keep`.
pub async fn remove(hub: &Arc<Hub>, cd: &Arc<Cd>, actor: &str, id: &str, keep: bool) -> Result<(), String> {
    if !cd.apps.read().contains_key(id) {
        return Ok(());
    }
    op(hub, cd, actor, "cd.apps.delete", json!({ "app": id, "cascade": if keep { "orphan" } else { "foreground" } })).await.map(|_| ())
}

/// Pauses or resumes KilnCD's automation for an app, leaving git as it is.
pub async fn automation(hub: &Arc<Hub>, cd: &Arc<Cd>, actor: &str, id: &str, on: bool) -> Result<(), String> {
    let Some(mut def) = cd.apps.read().get(id).map(|e| e.def.clone()) else { return Ok(()) };
    def.sync.auto = on;
    op(hub, cd, actor, "cd.apps.create", json!({ "definition": def })).await.map(|_| ())
}

/// What KilnCD says of an app, as a release follows it: whether the commit has landed, and the
/// status to show. The definition is the release's (the caller made sure): an error rendering it
/// is the release's.
pub fn landed(cd: &Cd, id: &str, commit: &str, since_ms: u64) -> (Landed, Value) {
    let status = cd.statuses.read().get(id).map(crate::cd::summary_json).unwrap_or(Value::Null);
    let (entry_error, at_commit) = cd.apps.read().get(id).map_or((None, false), |e| (e.error.clone(), e.commits.iter().flatten().any(|c| c.as_str() == commit)));
    let history = cd.store.read(|d| d.history.get(id).cloned()).unwrap_or_default();
    // The newest sync of this commit since the release began.
    let ran = history.iter().rev().find(|h| h.at_ms >= since_ms && h.commits.iter().flatten().any(|c| c.as_str() == commit)).cloned();
    let s = |k: &str| status.get(k).and_then(Value::as_str).unwrap_or("").to_owned();
    // Its sources can't be read (a chart version that isn't there), or don't render.
    let refused = entry_error.clone().or_else(|| Some(s("message")).filter(|m| m.starts_with("render failed")));
    let view = json!({
        "app": id,
        "sync": s("sync"),
        "health": s("health"),
        "message": status.get("message").cloned().unwrap_or(Value::Null),
        "operation": status.get("operation").cloned().unwrap_or(Value::Null),
        "source_error": entry_error,
        "last_sync": ran.as_ref().map(|h| json!({ "succeeded": h.succeeded, "message": h.message, "at_ms": h.at_ms, "id": h.id })),
    });
    let settled = s("sync") == "Synced" && s("health") == "Healthy";
    let syncing = status.pointer("/operation/phase").and_then(Value::as_str).is_some_and(|p| matches!(p, "DryRun" | "Running" | "Failing" | "Terminating"));
    let landed = match (ran, refused) {
        (Some(h), _) if !h.succeeded => Landed::Failed(if h.message.is_empty() { "KilnCD's sync of the release failed".to_owned() } else { h.message }),
        (Some(_), _) if settled => Landed::Yes,
        (None, Some(why)) => Landed::Refused(why),
        // Nothing to sync: KilnCD read the release's commit, and what it renders runs already.
        (None, None) if at_commit && settled && !syncing => Landed::Yes,
        _ => Landed::No,
    };
    (landed, view)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::deploy::model::{Artifact, RepoRef, SCHEMA, Targets};

    fn env() -> Environment {
        Environment {
            name: "staging".into(),
            provider: "kilncd".into(),
            release_repo: RepoRef::default(),
            targets: Targets { kubernetes: vec!["dev".into()], ..Targets::default() },
            gates: Default::default(),
            sync: Default::default(),
            watch: Default::default(),
            concurrency: Default::default(),
            agents: Default::default(),
        }
    }

    fn app() -> App {
        App {
            name: "web".into(),
            project: "shop".into(),
            owners: vec![],
            package: Package::Helm {
                repo: "oci://ghcr.io/acme/charts".into(),
                chart: "web".into(),
                chart_version: None,
                set: BTreeMap::from([("image.tag".into(), "${{ release.artifacts.image.tag }}".into()), ("replicas".into(), "2".into())]),
            },
            namespace: Some("shop-web".into()),
            properties: BTreeMap::new(),
            template: None, contacts: Default::default(), archived: false,
        }
    }

    fn file() -> ReleaseFile {
        ReleaseFile {
            schema: SCHEMA,
            project: "shop".into(),
            app: "web".into(),
            environment: "staging".into(),
            release: 7,
            enabled: true,
            version: Some("1.4.2".into()),
            package: app().package,
            artifacts: BTreeMap::from([("image".into(), Artifact { repository: "ghcr.io/acme/web".into(), tag: "1.10".into(), digest: String::new() })]),
            requested_by: "ada".into(),
            at: String::new(),
            rollback_of: None,
        }
    }

    #[test]
    fn a_release_becomes_a_definition_pinned_to_its_commit() -> Result<(), String> {
        let reg = Registration { app: "web".into(), environment: "staging".into(), enabled: true, paused: false, running: None, created_at: 0 };
        let def = definition(&app(), &env(), &reg, &file(), "abc123", "file:///data/releases.git")?;
        assert_eq!(def.id(), "shop/web-staging");
        assert_eq!((def.destination.cluster.as_str(), def.destination.namespace.as_str()), ("dev", "shop-web"));
        let [package, release] = def.sources.as_slice() else { return Err("two sources".into()) };
        assert_eq!((package.chart.as_deref(), package.revision.as_str()), (Some("web"), "1.4.2"), "the chart at the release's version");
        assert_eq!((release.revision.as_str(), release.as_ref.as_deref()), ("abc123", Some("release")), "the release repository at the release's commit");
        let Engine::Helm(h) = &package.engine else { return Err("a Helm chart".into()) };
        assert_eq!(h.value_files, ["$release/shop/web/staging/values.yaml"]);
        let tag = h.parameters.iter().find(|p| p.name == "image.tag").ok_or("image.tag")?;
        assert!(tag.value == "1.10" && tag.force_string, "a release's value stays a string");
        let replicas = h.parameters.iter().find(|p| p.name == "replicas").ok_or("replicas")?;
        assert!(!replicas.force_string, "a literal is Helm's to read");
        assert!(def.sync.auto && def.sync.options.create_namespace == Some(true));
        let paused = Registration { paused: true, ..reg };
        assert!(!definition(&app(), &env(), &paused, &file(), "abc123", "file:///x")?.sync.auto, "a paused registration isn't synced automatically");
        Ok(())
    }

    #[test]
    fn a_platform_release_becomes_a_definition_kilncd_renders_for_its_target() -> Result<(), String> {
        let lambda = Package::Platform {
            provider: "aws-lambda".into(),
            spec: json!({
                "function": "thumbs-${{ env.name }}",
                "code": "s3://builds/thumbs-${{ release.version }}.zip",
                "set": { "Runtime": "python3.13", "Handler": "app.handler", "Role": "arn:aws:iam::1:role/thumbs", "Description": "${{ git.short_sha }}" }
            }),
            repo: None,
            path: String::new(),
        };
        let a = App { package: lambda, ..app() };
        let mut e = env();
        e.targets.platforms.insert("aws-lambda".into(), serde_json::from_value(json!({ "account": "aws-prod", "region": "us-east-1" })).map_err(|e| e.to_string())?);
        let reg = Registration { app: "web".into(), environment: "staging".into(), enabled: true, paused: false, running: None, created_at: 0 };
        let f = ReleaseFile { package: a.package.clone(), ..file() };
        let def = definition(&a, &e, &reg, &f, "abc123", "file:///data/hosted/kiln/releases.git")?;
        assert_eq!(def.destination.cluster, "aws-lambda/aws-prod/us-east-1", "the target is the engine's cluster");
        let [package, release] = def.sources.as_slice() else { return Err("two sources".into()) };
        let Engine::Provider(p) = &package.engine else { return Err("a provider's source".into()) };
        assert_eq!(p.spec.pointer("/function").and_then(Value::as_str), Some("thumbs-staging"));
        assert_eq!(p.spec.pointer("/code").and_then(Value::as_str), Some("s3://builds/thumbs-1.4.2.zip"));
        assert_eq!(p.spec.pointer("/set/Description").and_then(Value::as_str), Some("${{ git.short_sha }}"), "KilnCD's own, left to it");
        assert_eq!((package.revision.as_str(), release.as_ref.as_deref()), ("abc123", Some("release")));
        assert!(def.sync.options.create_namespace.is_none(), "a namespace is Kubernetes'");
        // KilnCD renders it.
        let read = |_: &str| None;
        let input = kiln_cd::providers::RenderInput { spec: &p.spec, target: &p.target, app: &def.name, namespace: &def.destination.namespace, revision: "abc123", read: &read };
        let objects = kiln_cd::providers::render("aws-lambda", &input)?;
        assert!(objects.iter().any(|o| o.get("kind").and_then(Value::as_str) == Some("Function")), "{objects:?}");
        // An environment without the target says so.
        assert!(definition(&a, &env(), &reg, &f, "abc", "file:///x").is_err_and(|m| m.contains("no aws-lambda target")));
        Ok(())
    }

    #[test]
    fn a_release_without_a_version_has_nothing_to_deploy() {
        let reg = Registration { app: "web".into(), environment: "staging".into(), enabled: true, paused: false, running: None, created_at: 0 };
        let unversioned = ReleaseFile { version: None, ..file() };
        assert!(definition(&app(), &env(), &reg, &unversioned, "abc", "file:///x").is_err());
    }
}
