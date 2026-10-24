//! `digitalocean` (docs/cd-targets.md §3): an App Platform app, each release its app spec.
//!
//! ```yaml
//! web:
//!   digitalocean:
//!     app: shop
//!     spec: .do/app.yaml                    # the app spec (found there by default), or an object
//!     image: "registry.digitalocean.com/acme/web@${{ release.artifacts.image.digest }}"
//!     component: web                        # the service the image is for (the first, by default)
//! ```
//!
//! - **App** (`shop`): `spec` is the app spec as App Platform spells it, its name the app's. `image`
//!   sets a service's `image` (DOCR, Docker Hub or GHCR, by its host). App Platform builds and
//!   deploys each change, the deployment before serving until it's active. A rollback to a
//!   release kiln deployed before is App Platform's rollback to that deployment.
//! - **Health** is the deployments': active, in progress, or the newest one's error.

use serde_json::{Value, json};

use super::{RenderInput, apply_sets, file_or_object, object, status, text};
use crate::health::Status;
use crate::model::Health;

pub const GROUP: &str = "apps.digitalocean.kiln.dev";

/// The app spec's place when the definition names none.
pub const DEFAULT_SPEC: &str = ".do/app.yaml";

/// The files a render reads.
pub fn files(spec: &Value) -> Vec<String> {
    vec![text(spec, "spec").unwrap_or(DEFAULT_SPEC).to_owned()]
}

/// An image as App Platform's spec names one: `{registry_type, registry, repository, tag|digest}`.
pub fn image_spec(image: &str) -> Result<Value, String> {
    let (name, at) = match image.split_once('@') {
        Some((n, d)) => (n, json!({ "digest": d })),
        None => match image.rsplit_once(':').filter(|(_, t)| !t.contains('/')) {
            Some((n, t)) => (n, json!({ "tag": t })),
            None => (image, json!({ "tag": "latest" })),
        },
    };
    let parts: Vec<&str> = name.split('/').collect();
    let (kind, registry, repository) = match parts.as_slice() {
        ["registry.digitalocean.com", reg, repo @ ..] if !repo.is_empty() => ("DOCR", Some(*reg), repo.join("/")),
        ["ghcr.io", owner, repo @ ..] if !repo.is_empty() => ("GHCR", Some(*owner), repo.join("/")),
        ["docker.io", owner, repo @ ..] if !repo.is_empty() => ("DOCKER_HUB", Some(*owner), repo.join("/")),
        [owner, repo] if !owner.contains('.') => ("DOCKER_HUB", Some(*owner), (*repo).to_owned()),
        [repo] => ("DOCKER_HUB", Some("library"), (*repo).to_owned()),
        _ => return Err(format!("image `{image}`: App Platform takes DOCR (registry.digitalocean.com/…), Docker Hub or GHCR images")),
    };
    let mut v = json!({ "registry_type": kind, "repository": repository });
    if let (Some(r), Value::Object(m)) = (registry, &mut v) {
        m.insert("registry".into(), json!(r));
    }
    if let (Value::Object(m), Value::Object(a)) = (&mut v, at) {
        m.extend(a);
    }
    Ok(v)
}

/// An app's objects.
pub fn render(input: &RenderInput<'_>) -> Result<Vec<Value>, String> {
    let app = text(input.spec, "app").ok_or("digitalocean needs app: the app's name")?;
    let mut spec = match input.spec.get("spec") {
        Some(_) => file_or_object(input, "spec")?.ok_or("spec: the app spec")?,
        None => {
            let bytes = (input.read)(DEFAULT_SPEC).ok_or("digitalocean needs the app spec: .do/app.yaml in the source, or spec:")?;
            serde_yaml::from_slice(&bytes).map_err(|e| format!("{DEFAULT_SPEC}: {e}"))?
        }
    };
    // `doctl apps spec get` writes the spec under `spec:`.
    if let Some(inner) = spec.get("spec").filter(|s| s.is_object()).cloned() {
        spec = inner;
    }
    let m = spec.as_object_mut().ok_or("the app spec: a map")?;
    m.insert("name".into(), json!(app));
    if let Some(image) = text(input.spec, "image") {
        let want = text(input.spec, "component");
        let services = m.get_mut("services").and_then(Value::as_array_mut).ok_or("image: the app spec has no services to give it to")?;
        let svc = match want {
            Some(c) => services.iter_mut().find(|s| s.get("name").and_then(Value::as_str) == Some(c)).ok_or_else(|| format!("component {c}: no such service in the app spec"))?,
            None => services.first_mut().ok_or("image: the app spec has no services")?,
        };
        let s = svc.as_object_mut().ok_or("a service: a map")?;
        s.remove("github");
        s.remove("gitlab");
        s.remove("git");
        s.insert("image".into(), image_spec(image)?);
    }
    apply_sets(&mut spec, input.spec.get("set"))?;
    Ok(vec![object(GROUP, "App", app, None, spec)])
}

/// An App object from App Platform's app and its deployments (newest first).
pub fn live_app(app: &Value, deployments: &[Value]) -> Option<Value> {
    let name = app.pointer("/spec/name")?.as_str()?;
    let mut o = object(GROUP, "App", name, None, app.get("spec").cloned().unwrap_or(Value::Null));
    let newest = deployments.first().map(|d| json!({ "id": d.get("id"), "phase": d.get("phase"), "cause": d.get("cause") }));
    if let Value::Object(m) = &mut o {
        m.insert("status".into(), json!({ "id": app.get("id"), "liveUrl": app.get("live_url"), "active": app.pointer("/active_deployment/id"), "inProgress": app.pointer("/in_progress_deployment/id"), "deployment": newest }));
    }
    Some(o)
}

pub fn health(o: &Value) -> Option<Status> {
    if crate::obj::kind(o) != "App" {
        return None;
    }
    let st = o.get("status")?;
    if st.get("inProgress").is_some_and(|v| !v.is_null()) {
        return status(Health::Progressing, "App Platform is deploying it");
    }
    match st.pointer("/deployment/phase").and_then(Value::as_str).unwrap_or_default() {
        "ACTIVE" => status(Health::Healthy, ""),
        p @ ("ERROR" | "CANCELED") => status(Health::Degraded, format!("its newest deployment: {p}")),
        "" => status(Health::Progressing, "nothing deployed yet"),
        p => status(Health::Progressing, format!("deploying: {p}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn images_name_their_registries() -> Result<(), String> {
        assert_eq!(image_spec("registry.digitalocean.com/acme/web@sha256:aa")?, json!({ "registry_type": "DOCR", "registry": "acme", "repository": "web", "digest": "sha256:aa" }));
        assert_eq!(image_spec("ghcr.io/acme/shop/web:1.2")?, json!({ "registry_type": "GHCR", "registry": "acme", "repository": "shop/web", "tag": "1.2" }));
        assert_eq!(image_spec("nginx")?, json!({ "registry_type": "DOCKER_HUB", "registry": "library", "repository": "nginx", "tag": "latest" }));
        assert!(image_spec("quay.io/acme/web:1").is_err());
        Ok(())
    }
}
