//! A release checked before its commit (docs/deploy-api.md §8.1): what can be known to fail is
//! refused while nothing has changed. Its secrets' pinned versions still have their values; its
//! images are in their registries; on KilnCD, its package renders as KilnCD will render it, and
//! its cluster takes every object in a server-side dry run. What kiln can't ask (a registry or a
//! cluster it can't reach, a namespace or a kind the sync makes first) lets the release through:
//! its provider has the last word.

use std::collections::BTreeSet;
use std::sync::Arc;

use kiln_cd::apps::{AppDef, Engine, Source};
use kiln_cd::obj;
use serde_json::Value;

use super::model::{App, Environment, Package, Registration, ReleaseFile};
use super::{Deploy, kilncd};
use crate::cd::Cd;
use crate::cd::kube::Cluster;
use crate::ci::integrations::{self, oci};
use crate::hub::Hub;

/// The field manager KilnCD applies as: a dry run as it, as a sync would apply.
const MANAGER: &str = "kiln";

/// What a release's preflight found.
#[derive(Debug, Default)]
pub struct Found {
    /// Why it can't work: it's refused, nothing changed.
    pub refused: Option<String>,
    /// What it runs on its way in that a revert couldn't undo (a migration's hook).
    pub irreversible: Option<String>,
}

/// What a release, as `file` says it and with `values` (the environment's values.yaml it commits;
/// `None`: the ones there now), meets before its commit.
pub async fn check(d: &Deploy, a: &App, env: &Environment, reg: &Registration, file: &ReleaseFile, values: Option<&str>) -> Found {
    let refused = |why: Option<String>| Found { refused: why, irreversible: None };
    if let Some(why) = secrets(&d.hub, a, env, file).await {
        return refused(Some(why));
    }
    // Argo CD renders from git alone, once the commit is there: its images are what kiln can ask.
    if env.provider.starts_with("argocd/") || !file.enabled || file.version.is_none() || matches!(a.package, Package::Platform { .. }) {
        return refused(missing_image(&d.hub, artifact_images(file)).await);
    }
    let Some(cd) = crate::cd::get() else { return refused(missing_image(&d.hub, artifact_images(file)).await) };
    rendered(d, &cd, a, env, reg, file, values).await
}

/// The Jobs and Pods a release runs on its way in (a hook before, during or after its sync: a
/// migration) that a revert couldn't undo. One marked `kiln.dev/reversible: "true"` (a change what
/// ran before also runs on: expand, then contract) doesn't count.
fn irreversible(objects: &[Value]) -> Option<String> {
    let hooks: Vec<String> = objects
        .iter()
        .filter(|o| matches!((obj::group(o), obj::kind(o)), ("batch", "Job") | ("", "Pod")))
        .filter(|o| {
            let on = |key: &str| obj::annotation(o, key).unwrap_or("").split(',').map(str::trim).map(str::to_owned).collect::<Vec<_>>();
            let helm = on("helm.sh/hook").iter().any(|h| matches!(h.as_str(), "pre-install" | "pre-upgrade" | "post-install" | "post-upgrade"));
            let argo = on("argocd.argoproj.io/hook").iter().any(|h| matches!(h.as_str(), "PreSync" | "Sync" | "PostSync"));
            (helm || argo) && obj::annotation(o, "kiln.dev/reversible") != Some("true")
        })
        .map(|o| format!("{} {}", obj::kind(o), obj::name(o)))
        .collect();
    (!hooks.is_empty()).then(|| hooks.join(", "))
}

/// The pinned secret versions with no value kept: a delivery of them would fail.
async fn secrets(hub: &Arc<Hub>, a: &App, env: &Environment, file: &ReleaseFile) -> Option<String> {
    let pins = crate::secrets::parse_versions(&file.secrets).ok().filter(|p| !p.is_empty())?;
    let missing = crate::secrets::of(hub).missing(&crate::secrets::Scope::of(&a.name, &env.name), &pins).await.ok()?;
    (!missing.is_empty()).then(|| format!("the values of secret {} are no longer kept: set them again", missing.iter().map(|(n, v)| format!("{n} v{v}")).collect::<Vec<_>>().join(", ")))
}

/// The release's artifacts as image references, by digest where known.
fn artifact_images(file: &ReleaseFile) -> Vec<String> {
    file.artifacts
        .values()
        .filter(|a| !a.repository.is_empty())
        .filter_map(|a| match (a.digest.is_empty(), a.tag.is_empty()) {
            (false, _) => Some(format!("{}@{}", a.repository, a.digest)),
            (true, false) => Some(format!("{}:{}", a.repository, a.tag)),
            (true, true) => None,
        })
        .collect()
}

/// The images an object's pods run.
fn images_of(o: &Value) -> Vec<String> {
    let at: &[&str] = match (obj::group(o), obj::kind(o)) {
        ("apps", "Deployment" | "StatefulSet" | "DaemonSet" | "ReplicaSet") | ("batch", "Job") => &["spec", "template", "spec"],
        ("batch", "CronJob") => &["spec", "jobTemplate", "spec", "template", "spec"],
        ("", "Pod") => &["spec"],
        _ => return Vec::new(),
    };
    let Some(pod) = obj::get(o, at) else { return Vec::new() };
    ["containers", "initContainers"]
        .iter()
        .flat_map(|k| obj::list_at(pod, &[*k]).into_iter().flatten())
        .filter_map(|c| c.get("image").and_then(Value::as_str))
        .map(str::to_owned)
        .collect()
}

/// The first image its registry says isn't there. A registry that won't say (unreachable, private
/// to kiln) lets it through; one that answers for its repository, without the image, doesn't.
async fn missing_image(hub: &Arc<Hub>, images: impl IntoIterator<Item = String>) -> Option<String> {
    let mut seen = BTreeSet::new();
    for text in images {
        if !seen.insert(text.clone()) {
            continue;
        }
        let Ok(reference) = oci::parse_reference(&text) else { continue };
        let registry = match integrations::registry_for(hub, &reference.registry).await {
            Ok(Some(r)) => r,
            _ => oci::Registry::for_name(&reference.registry, None, false),
        };
        match oci::resolve(&registry, &reference).await {
            Ok(Some(_)) => {}
            // Not found: missing only if its repository answers (a registry may say "not found"
            // to whoever may not see it).
            Ok(None) => match oci::tags(&registry, &reference.repository).await {
                Ok(_) => return Some(format!("image {text} isn't in its registry")),
                Err(e) => tracing::debug!(image = %text, error = %format!("{e:#}"), "deploy: an image's repository can't be listed; the provider pulls it"),
            },
            Err(e) => tracing::debug!(image = %text, error = %format!("{e:#}"), "deploy: an image can't be looked up; the provider pulls it"),
        }
    }
    None
}

/// On KilnCD: the release rendered as KilnCD will render it (against the release repository as it
/// is, the values it commits given inline), each image it runs in its registry, and each object
/// taken by its cluster in a server-side dry run.
async fn rendered(d: &Deploy, cd: &Arc<Cd>, a: &App, env: &Environment, reg: &Registration, file: &ReleaseFile, values: Option<&str>) -> Found {
    let refused = |why: String| Found { refused: Some(why), irreversible: None };
    let Ok(at) = super::git::open(d, &env.release_repo).await else { return Found::default() };
    let Some(head) = at.head().await.ok().flatten() else { return Found::default() };
    let Ok(url) = at.url() else { return Found::default() };
    if kilncd::fetched(cd, &url, &head).await.is_err() {
        return Found::default();
    }
    let version = file.version.clone().unwrap_or_default();
    let chart = kilncd::git_chart(cd, &a.package, &version).await;
    let mut def = match kilncd::definition(a, env, reg, file, &head, &url, chart) {
        Ok(def) => def,
        Err(why) => return refused(why),
    };
    if let Some(v) = values {
        match serde_yaml::from_str::<Value>(v) {
            Ok(inline) => inline_values(&mut def.sources, inline),
            Err(e) => return refused(format!("its values aren't YAML: {e}")),
        }
    }
    let objects = match cd.render_definition(def.clone()).await {
        Ok(o) => o,
        Err(e) => return refused(format!("it doesn't render: {e:#}")),
    };
    let images = objects.iter().flat_map(images_of).chain(artifact_images(file));
    if let Some(why) = missing_image(&d.hub, images).await {
        return refused(why);
    }
    let irreversible = irreversible(&objects);
    let Some(cluster) = cd.clusters.read().get(&def.destination.cluster).cloned() else { return Found { refused: None, irreversible } };
    Found { refused: dry_run(&cluster, &def, &objects).await, irreversible }
}

/// The values a release commits, given to its Helm sources inline in place of the release
/// repository's file, which holds the ones before it until the commit.
fn inline_values(sources: &mut [Source], values: Value) {
    for s in sources {
        if let Engine::Helm(h) = &mut s.engine
            && h.value_files.iter().any(|f| f.starts_with("$release/"))
        {
            h.value_files.retain(|f| !f.starts_with("$release/"));
            h.values = Some(values.clone());
        }
    }
}

/// Each object as its cluster would take it from a sync: a server-side apply, run dry. One whose
/// namespace the sync makes first, whose kind comes with the release (its CRD), or that's a hook
/// (made at sync, often by generated name) can't be asked about yet, and isn't.
async fn dry_run(cluster: &Cluster, def: &AppDef, objects: &[Value]) -> Option<String> {
    let ns = def.destination.namespace.as_str();
    let ns_there = ns.is_empty() || matches!(cluster.get(&obj::ResKey::new("", "Namespace", "", ns)).await, Ok(Some(_)));
    for o in objects {
        let hook = obj::annotation(o, "helm.sh/hook").is_some() || obj::annotation(o, "argocd.argoproj.io/hook").is_some();
        if hook || obj::name(o).is_empty() {
            continue;
        }
        let Some(namespaced) = cluster.namespaced(obj::group(o), obj::kind(o)) else { continue };
        let mut o = o.clone();
        if namespaced {
            if !ns_there {
                continue;
            }
            if obj::namespace(&o).is_empty()
                && let Some(meta) = obj::ensure_map(&mut o, &["metadata"])
            {
                meta.insert("namespace".into(), Value::String(ns.to_owned()));
            }
        }
        match cluster.dry_run(&o, MANAGER).await {
            Ok(Ok(())) => {}
            Ok(Err(why)) => return Some(format!("{} {}: {why}", obj::kind(&o), obj::name(&o))),
            Err(e) => tracing::debug!(kind = obj::kind(&o), name = obj::name(&o), error = %format!("{e:#}"), "deploy: a dry run can't be asked; the sync will say"),
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::deploy::model::Artifact;
    use kiln_cd::apps::Helm;
    use serde_json::json;
    use std::collections::BTreeMap;

    #[test]
    fn a_release_s_images_are_its_artifacts_and_what_its_pods_run() -> Result<(), String> {
        let file = ReleaseFile {
            schema: 1,
            project: "shop".into(),
            app: "web".into(),
            environment: "staging".into(),
            release: 7,
            enabled: true,
            version: Some("1.4.2".into()),
            package: serde_json::from_value(json!({ "kind": "helm", "repo": "oci://ghcr.io/acme/charts", "chart": "web" })).map_err(|e| e.to_string())?,
            artifacts: BTreeMap::from([
                ("image".into(), Artifact { repository: "ghcr.io/acme/web".into(), tag: "1.4.2".into(), digest: String::new() }),
                ("worker".into(), Artifact { repository: "ghcr.io/acme/worker".into(), tag: "1.4.2".into(), digest: "sha256:9f".into() }),
            ]),
            requested_by: "ada".into(),
            at: String::new(),
            rollback_of: None,
            secrets: String::new(),
            namespace: None,
            set: BTreeMap::new(),
            helm: None,
        };
        assert_eq!(artifact_images(&file), ["ghcr.io/acme/web:1.4.2", "ghcr.io/acme/worker@sha256:9f"]);
        let deployment = json!({"apiVersion": "apps/v1", "kind": "Deployment", "spec": {"template": {"spec": {
            "initContainers": [{"name": "migrate", "image": "ghcr.io/acme/web:1.4.2"}],
            "containers": [{"name": "app", "image": "ghcr.io/acme/web:1.4.2"}, {"name": "proxy", "image": "envoyproxy/envoy:v1.31"}]}}}});
        assert_eq!(images_of(&deployment), ["ghcr.io/acme/web:1.4.2", "envoyproxy/envoy:v1.31", "ghcr.io/acme/web:1.4.2"]);
        let cron = json!({"apiVersion": "batch/v1", "kind": "CronJob", "spec": {"jobTemplate": {"spec": {"template": {"spec": {"containers": [{"name": "c", "image": "busybox:1.37"}]}}}}}});
        assert_eq!(images_of(&cron), ["busybox:1.37"]);
        assert!(images_of(&json!({"apiVersion": "v1", "kind": "Service"})).is_empty());
        Ok(())
    }

    #[test]
    fn hooks_on_the_way_in_are_what_a_revert_couldnt_undo() {
        let job = |name: &str, annotations: serde_json::Value| json!({"apiVersion": "batch/v1", "kind": "Job", "metadata": {"name": name, "annotations": annotations}});
        let objects = vec![
            job("web-migrate", json!({"helm.sh/hook": "pre-install,pre-upgrade"})),
            job("web-seed", json!({"argocd.argoproj.io/hook": "PostSync"})),
            job("web-expand", json!({"helm.sh/hook": "pre-upgrade", "kiln.dev/reversible": "true"})),
            job("web-test", json!({"helm.sh/hook": "test"})),
            job("web-cleanup", json!({"helm.sh/hook": "pre-delete"})),
            json!({"apiVersion": "v1", "kind": "ConfigMap", "metadata": {"name": "web-hooked", "annotations": {"helm.sh/hook": "pre-upgrade"}}}),
        ];
        assert_eq!(irreversible(&objects).as_deref(), Some("Job web-migrate, Job web-seed"));
        assert_eq!(irreversible(objects.get(2..).unwrap_or(&[])), None, "reversible, test, delete hooks and what doesn't run don't count");
    }

    #[test]
    fn the_values_a_release_commits_render_inline_in_place_of_the_repository_s() -> Result<(), String> {
        let helm = Helm { value_files: vec!["$release/shop/web/staging/values.yaml".into(), "values-extra.yaml".into()], ..Helm::default() };
        let mut sources = vec![Source { repo: "oci://ghcr.io/acme/charts".into(), path: String::new(), revision: "1.4.2".into(), chart: Some("web".into()), as_ref: None, engine: Engine::Helm(helm) }];
        inline_values(&mut sources, json!({ "replicaCount": 3 }));
        let Some(Source { engine: Engine::Helm(h), .. }) = sources.first() else { return Err("a Helm source".into()) };
        assert_eq!(h.value_files, ["values-extra.yaml"], "the release repository's file goes; the chart's own stay");
        assert_eq!(h.values, Some(json!({ "replicaCount": 3 })));
        Ok(())
    }
}
