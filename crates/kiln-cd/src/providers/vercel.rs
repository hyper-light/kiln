//! `vercel` (docs/cd-targets.md §3): a project, its production deployment from the prebuilt
//! output CI made (Vercel's Build Output API), and its domains.
//!
//! ```yaml
//! site:
//!   vercel:
//!     project: site
//!     prebuilt: "${{ artifacts.output }}"   # the zip of .vercel/output that `vercel build` made
//!     settings: { framework: nextjs }       # the project's settings, when kiln makes it
//!     domains: [www.example.com]
//! ```
//!
//! - **Project** (its name): its settings.
//! - **Production** (the project's name): `spec.output` is the output the production
//!   deployment serves, by its hash. A deployment is made from it (only files Vercel lacks are
//!   uploaded) and serves production once ready; a production deployment made outside kiln reads
//!   as that deployment: drift, or a rollback by hand.
//! - **Domain** (its name): on the project, after the deployment.

use serde_json::{Value, json};

use super::{RenderInput, object, status, text};
use crate::health::Status;
use crate::model::Health;

pub const GROUP: &str = "vercel.kiln.dev";

/// The files a render reads: none (Vercel reads vercel.json from the output itself).
pub fn files(_spec: &Value) -> Vec<String> {
    Vec::new()
}

/// An app's objects.
pub fn render(input: &RenderInput<'_>) -> Result<Vec<Value>, String> {
    let project = text(input.spec, "project").ok_or("vercel needs project: the project's name")?;
    let output = text(input.spec, "prebuilt").ok_or("vercel needs prebuilt: the zip of .vercel/output CI made (${{ artifacts.<name> }})")?;
    if !output.strip_prefix("sha256:").is_some_and(|h| h.len() == 64 && h.bytes().all(|b| b.is_ascii_hexdigit())) {
        return Err(format!("prebuilt `{output}`: a workflow's artifact (${{{{ artifacts.<name> }}}}), which resolves to sha256:<hex>"));
    }
    let settings = match input.spec.get("settings") {
        None | Some(Value::Null) => json!({}),
        Some(Value::Object(m)) => Value::Object(m.clone()),
        Some(_) => return Err("settings: the project's settings, an object (framework, buildCommand, …)".into()),
    };
    let mut out = vec![wave(object(GROUP, "Project", project, None, settings), 0), wave(object(GROUP, "Production", project, None, json!({ "project": project, "output": output })), 1)];
    // Domains belong to their project: it is their namespace.
    for d in input.spec.get("domains").and_then(Value::as_array).into_iter().flatten() {
        let name = d.as_str().ok_or("domains: names, like www.example.com")?;
        out.push(wave(object(GROUP, "Domain", name, Some(project), json!({ "project": project })), 2));
    }
    Ok(out)
}

fn wave(mut o: Value, w: i64) -> Value {
    if let Some(ann) = crate::obj::ensure_map(&mut o, &["metadata", "annotations"]) {
        ann.insert(crate::sync::KILN_WAVE.into(), Value::String(w.to_string()));
    }
    o
}

/// An output's files as a deployment takes them: under `.vercel/output/`, wherever the zip
/// rooted them.
pub fn output_path(name: &str, rooted: bool) -> String {
    if rooted { name.to_owned() } else { format!(".vercel/output/{name}") }
}

pub fn health(obj: &Value) -> Option<Status> {
    match crate::obj::kind(obj) {
        "Production" => match crate::obj::str_at(obj, &["status", "readyState"]) {
            Some("READY") => status(Health::Healthy, ""),
            Some("ERROR") => status(Health::Degraded, crate::obj::str_at(obj, &["status", "errorMessage"]).unwrap_or("the deployment failed")),
            Some("CANCELED") => status(Health::Degraded, "the deployment was canceled"),
            Some(s) => status(Health::Progressing, s.to_ascii_lowercase()),
            None => status(Health::Missing, "no production deployment"),
        },
        "Domain" => match obj.pointer("/status/verified").and_then(Value::as_bool) {
            Some(false) => status(Health::Progressing, "waiting for the domain's DNS to verify"),
            _ => status(Health::Healthy, ""),
        },
        "Project" | "Deployment" => status(Health::Healthy, ""),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use serde_json::Map;

    use super::*;

    #[test]
    fn a_definition_renders_project_production_and_domains() -> Result<(), String> {
        let hex = "c".repeat(64);
        let spec = json!({ "project": "site", "prebuilt": format!("sha256:{hex}"), "settings": { "framework": "nextjs" }, "domains": ["www.example.com"] });
        let none = Map::new();
        let objs = render(&RenderInput { spec: &spec, target: &none, app: "site", namespace: "", revision: "abc", read: &|_| None })?;
        let kinds: Vec<(&str, &str)> = objs.iter().map(|o| (crate::obj::kind(o), crate::obj::name(o))).collect();
        assert_eq!(kinds, [("Project", "site"), ("Production", "site"), ("Domain", "www.example.com")]);
        assert_eq!(objs.get(1).and_then(|o| o.pointer("/spec/output")), Some(&json!(format!("sha256:{hex}"))));
        let bad = json!({ "project": "site", "prebuilt": ".vercel/output" });
        assert!(render(&RenderInput { spec: &bad, target: &none, app: "site", namespace: "", revision: "abc", read: &|_| None }).is_err());
        assert_eq!(output_path("config.json", false), ".vercel/output/config.json");
        let p = |s: &str| json!({ "apiVersion": format!("{GROUP}/v1"), "kind": "Production", "status": { "readyState": s } });
        assert_eq!(health(&p("READY")).map(|s| s.health), Some(Health::Healthy));
        assert_eq!(health(&p("BUILDING")).map(|s| s.health), Some(Health::Progressing));
        assert_eq!(health(&p("ERROR")).map(|s| s.health), Some(Health::Degraded));
        Ok(())
    }
}
