//! An app's secrets where Argo CD deploys it (docs/deploy-api.md §3.5). Argo CD renders from git
//! alone, so the Secret can't be added at render: kiln writes `<app>-kiln` into the app's namespace
//! itself, where it reaches the cluster (one of KilnCD's clusters of this server, which the
//! environment's `targets.kilncd` names by Argo CD's name for it, or one KilnCD calls by that
//! name), before the release's commit reaches Argo CD: as it commits, or with `via:
//! pull-request` as its merge is seen (what the merge pins; an open pull request changes nothing
//! that runs). It carries none of Argo CD's tracking, so Argo CD neither shows it as the app's nor
//! prunes it. The release says its versions to the chart
//! (`kiln.secrets`), which kiln-base stamps on the pod template, so pods roll onto new ones. One
//! with none left has the Secret kiln wrote deleted.

use serde_json::Value;

use super::Deploy;
use super::model::{App, Environment};

/// The KilnCD cluster kiln writes an Argo CD environment's secrets to, for Argo CD's cluster
/// `argo`; `None`: kiln reaches no such cluster.
fn cluster_for(env: &Environment, argo: &str, known: impl Fn(&str) -> bool) -> Option<String> {
    env.targets.kilncd.get(argo).cloned().or_else(|| known(argo).then(|| argo.to_owned()))
}

/// Whether a Secret kiln wrote holds exactly the versions `pinned`.
fn holds(s: &Value, pinned: &[(String, u32)]) -> bool {
    let at = s.pointer("/metadata/annotations").and_then(|a| a.get(kiln_cd::apps::SECRETS)).and_then(Value::as_str).unwrap_or_default();
    let mut have = crate::secrets::parse_versions(at).unwrap_or_default();
    let mut want = pinned.to_vec();
    have.sort();
    want.sort();
    have == want && s.get("data").and_then(Value::as_object).is_some_and(|d| want.iter().all(|(n, _)| d.contains_key(n)) && d.len() == want.len())
}

/// Whether a Secret is the one kiln writes for `app`, by its labels.
fn ours(o: &Value, app: &App) -> bool {
    let label = |k: &str| o.get("metadata").and_then(|m| m.get("labels")).and_then(|l| l.get(k)).and_then(Value::as_str);
    label("app.kubernetes.io/managed-by") == Some("kiln") && label("kiln.dev/app") == Some(app.name.as_str())
}

/// The Secret `<app>-kiln` of `items` at `versions`, as kiln writes it for Argo CD.
pub fn secret<'a>(app: &App, versions: &str, items: impl IntoIterator<Item = (&'a str, &'a [u8])>) -> Value {
    let mut s = kiln_cd::apps::app_secret(&format!("{}-kiln", app.name), app.namespace(), versions, items);
    if let Some(meta) = s.get_mut("metadata").and_then(Value::as_object_mut) {
        meta.insert("labels".into(), serde_json::json!({ "app.kubernetes.io/managed-by": "kiln", "kiln.dev/app": app.name }));
        // Not the Application's (no tracking label or annotation), and extraneous to its comparison.
        if let Some(a) = meta.get_mut("annotations").and_then(Value::as_object_mut) {
            a.remove("argocd.argoproj.io/sync-wave");
            a.insert("argocd.argoproj.io/compare-options".into(), Value::String("IgnoreExtraneous".into()));
        }
    }
    s
}

/// Why kiln can't deliver an app's secrets (`versions`) where Argo CD deploys `env`, if it can't:
/// a refusal, which won't change by trying again.
pub fn unreachable(app: &App, env: &Environment, versions: &str) -> Option<String> {
    if versions.trim().is_empty() {
        return None;
    }
    let argo = super::argocd::cluster_of(env);
    let Some(cd) = crate::cd::get() else { return Some(format!("{}'s secrets go to Argo CD's {argo} through KilnCD, which isn't running here ([cd] enabled)", app.name)) };
    if cluster_for(env, &argo, |n| cd.has_cluster(n)).is_some() {
        return None;
    }
    Some(format!(
        "{} has secrets, and kiln reaches no cluster of Argo CD's {argo}: add it to KilnCD (kiln cd cluster add), and name it in the environment's targets.kilncd ({{ {argo}: <cluster> }})",
        app.name
    ))
}

/// What a release on Argo CD delivers of the app's secrets (`versions`, as `release.json` pins
/// them): `Ok(None)` written (or none to write, the one kiln wrote before deleted), `Ok(Some(why))`
/// refused (kiln reaches no cluster there: that won't change by trying again), `Err` not yet
/// (tried again).
pub async fn deliver(d: &Deploy, app: &App, env: &Environment, versions: &str) -> Result<Option<String>, String> {
    let argo = super::argocd::cluster_of(env);
    let cd = crate::cd::get();
    let cluster = cd.as_ref().and_then(|cd| cluster_for(env, &argo, |n| cd.has_cluster(n)));
    if versions.trim().is_empty() {
        // None left: the one kiln wrote goes, as well as it can (a release without secrets never
        // waits on the cluster).
        if let (Some(cd), Some(cluster)) = (&cd, &cluster) {
            let key = kiln_cd::obj::ResKey::new("", "Secret", app.namespace(), &format!("{}-kiln", app.name));
            if let Err(e) = cd.delete_from(cluster, &key, |o| ours(o, app)).await {
                tracing::warn!(app = %app.name, environment = %env.name, %cluster, error = %format!("{e:#}"), "deploy: the secrets kiln wrote for Argo CD aren't deleted yet");
            }
        }
        return Ok(None);
    }
    let (Some(cd), Some(cluster)) = (cd, cluster) else { return Ok(unreachable(app, env, versions)) };
    let pinned = crate::secrets::parse_versions(versions).map_err(|e| format!("{e:#}"))?;
    // Already there at these versions (a release that changes something else): nothing read.
    let key = kiln_cd::obj::ResKey::new("", "Secret", app.namespace(), &format!("{}-kiln", app.name));
    let there = cd.get_from(&cluster, &key).await.map_err(|e| format!("{}-kiln in {cluster}: {e:#}", app.name))?;
    if there.as_ref().is_some_and(|s| ours(s, app) && holds(s, &pinned)) {
        return Ok(None);
    }
    let why = crate::secrets::Use { by: "kiln".into(), purpose: format!("deliver {} in {} to Argo CD's {argo} (through {cluster})", app.name, env.name) };
    let bundle = crate::secrets::of(&d.hub).resolve(&crate::secrets::Scope::of(&app.name, &env.name), &pinned, &why).await.map_err(|e| format!("the app's secrets: {e:#}"))?;
    let object = secret(app, &bundle.versions(), bundle.items.iter().map(|(n, _, v)| (n.as_str(), v.bytes())));
    cd.apply_to(&cluster, &object, "kiln-deploy").await.map_err(|e| format!("{}-kiln into {cluster}: {e:#}", app.name))?;
    Ok(None)
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use super::*;
    use crate::deploy::model::Package;

    #[test]
    fn a_secret_argo_cd_neither_tracks_nor_prunes() -> anyhow::Result<()> {
        let app = App {
            name: "web".into(),
            project: "shop".into(),
            owners: vec![],
            package: Package::Git { repo: "x".into(), path: String::new(), set: BTreeMap::new() },
            namespace: Some("shop-web".into()),
            properties: BTreeMap::new(),
            template: None,
            contacts: Default::default(),
            source: None,
            archived: false,
            registries: None,
        };
        let s = secret(&app, "DB_PASSWORD@2", [("DB_PASSWORD", b"hunter2".as_slice())]);
        assert_eq!((s.pointer("/metadata/name"), s.pointer("/metadata/namespace")), (Some(&serde_json::json!("web-kiln")), Some(&serde_json::json!("shop-web"))));
        assert_eq!(s.pointer("/metadata/annotations/kiln.dev~1secrets"), Some(&serde_json::json!("DB_PASSWORD@2")));
        assert_eq!(s.pointer("/metadata/annotations/argocd.argoproj.io~1compare-options"), Some(&serde_json::json!("IgnoreExtraneous")));
        assert!(s.pointer("/metadata/labels/app.kubernetes.io~1instance").is_none() && s.pointer("/metadata/annotations/argocd.argoproj.io~1tracking-id").is_none(), "none of Argo CD's tracking: {s}");
        assert_eq!(s.pointer("/data/DB_PASSWORD"), Some(&serde_json::json!("aHVudGVyMg==")));
        // Only the one kiln wrote for the app is deleted once it has none; one at the versions a
        // release pins is left as it is.
        assert!(ours(&s, &app));
        assert!(holds(&s, &[("DB_PASSWORD".into(), 2)]) && !holds(&s, &[("DB_PASSWORD".into(), 3)]) && !holds(&s, &[("DB_PASSWORD".into(), 2), ("API_KEY".into(), 1)]));
        assert!(!ours(&serde_json::json!({ "metadata": { "labels": { "kiln.dev/app": "web" } } }), &app));
        // Which cluster: the environment's say, else KilnCD's of the same name.
        let mut env: Environment = serde_json::from_value(serde_json::json!({ "name": "production", "provider": "argocd/prod", "targets": { "kubernetes": ["in-cluster"], "kilncd": { "in-cluster": "prod-k" } } }))?;
        assert_eq!(cluster_for(&env, "in-cluster", |_| false).as_deref(), Some("prod-k"));
        env.targets.kilncd.clear();
        assert_eq!(cluster_for(&env, "in-cluster", |n| n == "in-cluster").as_deref(), Some("in-cluster"));
        assert_eq!(cluster_for(&env, "in-cluster", |_| false), None);
        Ok(())
    }
}
