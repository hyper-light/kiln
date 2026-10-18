//! `cloudflare-workers` (docs/cd-targets.md §3): a Worker (the bundle CI built, its settings,
//! bindings, Durable Object migrations and cron triggers), its routes and its custom domains,
//! read from the app's own wrangler config so it moves to kiln without rewriting.
//!
//! ```yaml
//! edge:
//!   source: workers/edge                 # wrangler.jsonc (or .json, .toml) is here
//!   cloudflare-workers:
//!     bundle: "${{ artifacts.bundle }}"  # the zip CI built: the Worker's modules
//!     main: index.js                     # the entry module in it (default: the config's main, built)
//!     env: production                    # a wrangler environment to read (default: the top level)
//!     confirm-delete: [OldCounter]       # Durable Object classes a migration may delete, with their data
//! ```
//!
//! - **Worker** (its name): `spec` holds what an upload's metadata says (`main_module`,
//!   `compatibility_date`, `compatibility_flags`, `bindings`), the bundle by its hash, the
//!   config's Durable Object `migrations` (all of them; the server sends the steps after the tag
//!   the Worker has), `crons` and `workers_dev`.
//! - **Durable Objects.** A version with a migration deploys at 100%, never split. A migration
//!   that deletes a class deletes its data, so the definition must name the class in
//!   `confirm-delete`, in git, before kiln renders it.
//! - **Route** (its pattern) and **CustomDomain** (its hostname), after the Worker.
//!
//! Secrets stay as they are: uploads keep the Worker's secret bindings (`wrangler secret put`,
//! or kiln's secrets, set apart from the bundle).

use serde_json::{Map, Value, json};

use super::{RenderInput, object, status, text};
use crate::health::Status;
use crate::model::Health;

pub const GROUP: &str = "workers.cloudflare.kiln.dev";

/// Where wrangler looks for its config, in its order.
const CONFIGS: &[&str] = &["wrangler.json", "wrangler.jsonc", "wrangler.toml"];

/// Keys an environment inherits from the top level (wrangler's inheritable keys); the rest
/// (bindings, vars) an environment says for itself.
const INHERITED: &[&str] = &["main", "compatibility_date", "compatibility_flags", "workers_dev", "route", "routes", "triggers", "observability", "placement", "limits", "logpush", "tail_consumers", "migrations"];

/// The files a render reads: the config named, or wrangler's candidates.
pub fn files(spec: &Value) -> Vec<String> {
    match text(spec, "config") {
        Some(c) => vec![c.to_owned()],
        None => CONFIGS.iter().map(|c| (*c).to_owned()).collect(),
    }
}

/// JSON with comments and trailing commas (wrangler.jsonc) as plain JSON.
pub fn jsonc(text: &str) -> String {
    // Comments out, strings kept as they are.
    let chars: Vec<char> = text.chars().collect();
    let mut bare: Vec<char> = Vec::with_capacity(chars.len());
    let mut i = 0usize;
    let mut in_str = false;
    while let Some(&c) = chars.get(i) {
        let next = chars.get(i + 1).copied();
        if in_str {
            bare.push(c);
            if c == '\\' {
                if let Some(n) = next {
                    bare.push(n);
                }
                i += 2;
                continue;
            }
            in_str = c != '"';
            i += 1;
            continue;
        }
        match (c, next) {
            ('"', _) => {
                in_str = true;
                bare.push(c);
                i += 1;
            }
            ('/', Some('/')) => {
                while chars.get(i).is_some_and(|c| *c != '\n') {
                    i += 1;
                }
            }
            ('/', Some('*')) => {
                i += 2;
                while chars.get(i).is_some() && !(chars.get(i) == Some(&'*') && chars.get(i + 1) == Some(&'/')) {
                    i += 1;
                }
                i += 2;
            }
            _ => {
                bare.push(c);
                i += 1;
            }
        }
    }
    // Trailing commas out: a comma whose next non-space character closes a list or an object.
    let mut out = String::with_capacity(bare.len());
    let mut in_str = false;
    let mut escaped = false;
    for (j, c) in bare.iter().enumerate() {
        if in_str {
            out.push(*c);
            if escaped {
                escaped = false;
            } else if *c == '\\' {
                escaped = true;
            } else if *c == '"' {
                in_str = false;
            }
            continue;
        }
        if *c == '"' {
            in_str = true;
        }
        if *c == ',' && bare.get(j + 1..).unwrap_or_default().iter().find(|x| !x.is_whitespace()).is_some_and(|x| *x == '}' || *x == ']') {
            continue;
        }
        out.push(*c);
    }
    out
}

/// A wrangler config by its file name.
pub fn parse_config(name: &str, bytes: &[u8]) -> Result<Value, String> {
    let text = String::from_utf8_lossy(bytes);
    if name.ends_with(".toml") {
        let t: toml::Value = toml::from_str(&text).map_err(|e| format!("{name}: {e}"))?;
        serde_json::to_value(t).map_err(|e| format!("{name}: {e}"))
    } else {
        serde_json::from_str(&jsonc(&text)).map_err(|e| format!("{name}: {e}"))
    }
}

/// The config an environment reads: its own keys, and the inheritable ones from the top level.
fn environment(config: &Value, env: Option<&str>) -> Result<Value, String> {
    let Some(env) = env else { return Ok(config.clone()) };
    let section = config.pointer(&format!("/env/{env}")).ok_or_else(|| format!("the wrangler config has no env.{env}"))?;
    let mut out = section.as_object().cloned().unwrap_or_default();
    for k in INHERITED {
        if let (false, Some(v)) = (out.contains_key(*k), config.get(*k)) {
            out.insert((*k).to_owned(), v.clone());
        }
    }
    // A named environment's Worker is `<name>-<env>` unless it names itself.
    if !out.contains_key("name")
        && let Some(n) = config.get("name").and_then(Value::as_str)
    {
        out.insert("name".into(), Value::String(format!("{n}-{env}")));
    }
    Ok(Value::Object(out))
}

/// The config's bindings as an upload's metadata takes them.
pub fn bindings(c: &Value) -> Result<Vec<Value>, String> {
    let mut out = Vec::new();
    let list = |k: &str| c.get(k).and_then(Value::as_array).cloned().unwrap_or_default();
    let s = |v: &Value, k: &str| v.get(k).and_then(Value::as_str).map(str::to_owned);
    if let Some(Value::Object(vars)) = c.get("vars") {
        for (k, v) in vars {
            out.push(match v {
                Value::String(t) => json!({ "type": "plain_text", "name": k, "text": t }),
                other => json!({ "type": "json", "name": k, "json": other }),
            });
        }
    }
    for kv in list("kv_namespaces") {
        out.push(json!({ "type": "kv_namespace", "name": s(&kv, "binding"), "namespace_id": s(&kv, "id") }));
    }
    for r2 in list("r2_buckets") {
        let mut b = json!({ "type": "r2_bucket", "name": s(&r2, "binding"), "bucket_name": s(&r2, "bucket_name") });
        if let (Some(j), Value::Object(m)) = (s(&r2, "jurisdiction"), &mut b) {
            m.insert("jurisdiction".into(), Value::String(j));
        }
        out.push(b);
    }
    for d1 in list("d1_databases") {
        out.push(json!({ "type": "d1", "name": s(&d1, "binding"), "id": s(&d1, "database_id") }));
    }
    for d in c.pointer("/durable_objects/bindings").and_then(Value::as_array).cloned().unwrap_or_default() {
        let mut b = json!({ "type": "durable_object_namespace", "name": s(&d, "name"), "class_name": s(&d, "class_name") });
        for k in ["script_name", "environment"] {
            if let (Some(x), Value::Object(m)) = (s(&d, k), &mut b) {
                m.insert(k.into(), Value::String(x));
            }
        }
        out.push(b);
    }
    for svc in list("services") {
        let mut b = json!({ "type": "service", "name": s(&svc, "binding"), "service": s(&svc, "service") });
        for k in ["environment", "entrypoint"] {
            if let (Some(x), Value::Object(m)) = (s(&svc, k), &mut b) {
                m.insert(k.into(), Value::String(x));
            }
        }
        out.push(b);
    }
    for q in c.pointer("/queues/producers").and_then(Value::as_array).cloned().unwrap_or_default() {
        out.push(json!({ "type": "queue", "name": s(&q, "binding"), "queue_name": s(&q, "queue") }));
    }
    for a in list("analytics_engine_datasets") {
        out.push(json!({ "type": "analytics_engine", "name": s(&a, "binding"), "dataset": s(&a, "dataset") }));
    }
    for h in list("hyperdrive") {
        out.push(json!({ "type": "hyperdrive", "name": s(&h, "binding"), "id": s(&h, "id") }));
    }
    for v in list("vectorize") {
        out.push(json!({ "type": "vectorize", "name": s(&v, "binding"), "index_name": s(&v, "index_name") }));
    }
    for (key, kind) in [("ai", "ai"), ("browser", "browser"), ("version_metadata", "version_metadata")] {
        if let Some(b) = c.get(key).and_then(|v| v.get("binding")).and_then(Value::as_str) {
            out.push(json!({ "type": kind, "name": b }));
        }
    }
    if c.get("assets").is_some() {
        return Err("static assets (`assets`) aren't deployed by kiln yet: serve them from Pages, or a bucket, for now".into());
    }
    for b in &out {
        if b.get("name").is_none_or(Value::is_null) {
            return Err(format!("a binding has no name: {b}"));
        }
    }
    sort_bindings(&mut out);
    Ok(out)
}

/// Bindings in name order (each name is one binding), so desired and live compare item by item.
pub fn sort_bindings(b: &mut [Value]) {
    b.sort_by(|x, y| x.get("name").and_then(Value::as_str).cmp(&y.get("name").and_then(Value::as_str)));
}

/// The config's Durable Object migrations, refusing a class deletion the definition doesn't
/// confirm (it deletes the class's data).
fn migrations(c: &Value, confirmed: &[String]) -> Result<Vec<Value>, String> {
    let list = c.get("migrations").and_then(Value::as_array).cloned().unwrap_or_default();
    for m in &list {
        let tag = m.get("tag").and_then(Value::as_str).ok_or("every migration needs its tag")?;
        for class in m.get("deleted_classes").and_then(Value::as_array).into_iter().flatten().filter_map(Value::as_str) {
            if !confirmed.iter().any(|c| c == class) {
                return Err(format!("migration {tag} deletes the Durable Object class {class}, and every object's data in it: name it in confirm-delete to go ahead"));
            }
        }
    }
    Ok(list)
}

/// An app's objects.
pub fn render(input: &RenderInput<'_>) -> Result<Vec<Value>, String> {
    let names = files(input.spec);
    let (file, bytes) = names.iter().find_map(|n| (input.read)(n).map(|b| (n.clone(), b))).ok_or_else(|| format!("no wrangler config in the source ({})", names.join(", ")))?;
    let config = parse_config(&file, &bytes)?;
    let c = environment(&config, text(input.spec, "env"))?;
    let name = text(input.spec, "name").map(str::to_owned).or_else(|| c.get("name").and_then(Value::as_str).map(str::to_owned)).ok_or("the Worker needs a name (in the wrangler config, or name:)")?;
    let bundle = text(input.spec, "bundle").ok_or("cloudflare-workers needs bundle: the zip CI built (${{ artifacts.<name> }})")?;
    if !bundle.strip_prefix("sha256:").is_some_and(|h| h.len() == 64 && h.bytes().all(|b| b.is_ascii_hexdigit())) {
        return Err(format!("bundle `{bundle}`: a workflow's artifact (${{{{ artifacts.<name> }}}}), which resolves to sha256:<hex>"));
    }
    // The entry module: said, or the config's main as a bundler writes it (src/index.ts → index.js).
    let main = text(input.spec, "main").map(str::to_owned).unwrap_or_else(|| {
        let m = c.get("main").and_then(Value::as_str).unwrap_or("index.js");
        let base = m.rsplit('/').next().unwrap_or(m);
        let stem = base.rsplit_once('.').map_or(base, |(s, _)| s);
        format!("{stem}.js")
    });
    let date = c.get("compatibility_date").and_then(Value::as_str).ok_or("the wrangler config needs compatibility_date")?;
    let confirmed: Vec<String> = input.spec.get("confirm-delete").and_then(Value::as_array).into_iter().flatten().filter_map(Value::as_str).map(str::to_owned).collect();
    let mut spec = Map::new();
    spec.insert("bundle".into(), Value::String(bundle.to_owned()));
    spec.insert("main_module".into(), Value::String(main));
    spec.insert("compatibility_date".into(), Value::String(date.to_owned()));
    spec.insert("compatibility_flags".into(), c.get("compatibility_flags").cloned().unwrap_or(json!([])));
    spec.insert("bindings".into(), Value::Array(bindings(&c)?));
    let migrations = migrations(&c, &confirmed)?;
    if !migrations.is_empty() {
        spec.insert("migrations".into(), Value::Array(migrations));
    }
    spec.insert("crons".into(), c.pointer("/triggers/crons").cloned().unwrap_or(json!([])));
    spec.insert("workers_dev".into(), c.get("workers_dev").cloned().unwrap_or(Value::Bool(true)));
    for k in ["observability", "placement", "limits", "logpush", "tail_consumers"] {
        if let Some(v) = c.get(k) {
            spec.insert(k.into(), v.clone());
        }
    }
    let mut out = vec![wave(object(GROUP, "Worker", &name, None, Value::Object(spec)), 0)];
    // Routes: patterns on a zone, or custom domains (`custom_domain: true`).
    let routes: Vec<Value> = match (c.get("routes"), c.get("route")) {
        (Some(Value::Array(r)), _) => r.clone(),
        (_, Some(r)) => vec![r.clone()],
        _ => Vec::new(),
    };
    for r in routes {
        let (pattern, zone, custom) = match &r {
            Value::String(p) => (p.clone(), None, false),
            Value::Object(o) => (
                o.get("pattern").and_then(Value::as_str).unwrap_or_default().to_owned(),
                o.get("zone_name").or_else(|| o.get("zone_id")).and_then(Value::as_str).map(str::to_owned),
                o.get("custom_domain").and_then(Value::as_bool).unwrap_or(false),
            ),
            _ => continue,
        };
        if pattern.is_empty() {
            return Err(format!("a route without a pattern: {r}"));
        }
        if custom {
            let mut s = json!({ "hostname": pattern, "service": name });
            if let (Some(z), Value::Object(m)) = (&zone, &mut s) {
                m.insert("zone_name".into(), Value::String(z.clone()));
            }
            out.push(wave(object(GROUP, "CustomDomain", &pattern, None, s), 1));
        } else {
            // A zone route needs its zone: said, or the pattern's host.
            let zone = zone.unwrap_or_else(|| pattern.split('/').next().unwrap_or_default().trim_start_matches("*.").trim_start_matches('*').to_owned());
            out.push(wave(object(GROUP, "Route", &pattern, None, json!({ "pattern": pattern, "zone_name": zone, "script": name })), 1));
        }
    }
    Ok(out)
}

fn wave(mut o: Value, w: i64) -> Value {
    if let Some(ann) = crate::obj::ensure_map(&mut o, &["metadata", "annotations"]) {
        ann.insert(crate::sync::KILN_WAVE.into(), Value::String(w.to_string()));
    }
    o
}

/// The migration steps after the tag a Worker has, and the tag they end at; none when it has
/// them all.
pub fn steps_after(migrations: &[Value], have: Option<&str>) -> Result<Option<(Vec<Value>, String)>, String> {
    let start = match have {
        None => 0,
        Some(t) => migrations.iter().position(|m| m.get("tag").and_then(Value::as_str) == Some(t)).map(|i| i + 1).ok_or_else(|| format!("the Worker is at migration {t}, which the config no longer lists"))?,
    };
    let rest = migrations.get(start..).unwrap_or_default();
    let Some(last) = rest.last().and_then(|m| m.get("tag")).and_then(Value::as_str) else { return Ok(None) };
    let steps = rest
        .iter()
        .map(|m| {
            let mut s = m.as_object().cloned().unwrap_or_default();
            s.remove("tag");
            Value::Object(s)
        })
        .collect();
    Ok(Some((steps, last.to_owned())))
}

/// A module's content type, by its name, as the upload takes it.
pub fn module_type(name: &str) -> &'static str {
    match name.rsplit('.').next().unwrap_or_default() {
        "js" | "mjs" => "application/javascript+module",
        "cjs" => "application/javascript",
        "wasm" => "application/wasm",
        "map" => "application/source-map",
        "json" => "application/json",
        "txt" | "html" | "md" | "css" | "svg" => "text/plain",
        _ => "application/octet-stream",
    }
}

pub fn health(obj: &Value) -> Option<Status> {
    match crate::obj::kind(obj) {
        "Worker" => {
            let versions = obj.pointer("/status/deployment/versions").and_then(Value::as_array).cloned().unwrap_or_default();
            if versions.len() > 1 {
                let split: Vec<String> = versions.iter().map(|v| format!("{} at {}%", v.get("version_id").and_then(Value::as_str).map(|i| i.get(..8).unwrap_or(i)).unwrap_or("?"), v.get("percentage").and_then(Value::as_f64).unwrap_or(0.0))).collect();
                return status(Health::Progressing, format!("deploying gradually: {}", split.join(", ")));
            }
            if versions.is_empty() {
                return status(Health::Missing, "no deployment");
            }
            status(Health::Healthy, "")
        }
        "Route" | "CustomDomain" | "Version" | "Deployment" => status(Health::Healthy, ""),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const HEX: &str = "abababababababababababababababababababababababababababababababab";

    fn render_with(config: &'static str, file: &'static str, spec: Value) -> Result<Vec<Value>, String> {
        let read = move |p: &str| (p == file).then(|| config.as_bytes().to_vec());
        let none = Map::new();
        render(&RenderInput { spec: &spec, target: &none, app: "edge", namespace: "", revision: "abc", read: &read })
    }

    #[test]
    fn jsonc_reads_as_json() {
        let text = "{\n  // a comment\n  \"name\": \"edge\", /* another */\n  \"url\": \"https://a//b\",\n  \"list\": [1, 2,],\n}\n";
        let v: Value = serde_json::from_str(&jsonc(text)).unwrap_or(Value::Null);
        assert_eq!(v, json!({ "name": "edge", "url": "https://a//b", "list": [1, 2] }));
    }

    #[test]
    fn a_wrangler_config_renders_a_worker_its_bindings_and_routes() -> Result<(), String> {
        let config = r#"{
          "name": "edge", "main": "src/index.ts", "compatibility_date": "2026-09-01", "compatibility_flags": ["nodejs_compat"],
          "vars": { "MODE": "prod", "LIMITS": { "rps": 10 } },
          "kv_namespaces": [{ "binding": "CACHE", "id": "kv1" }],
          "durable_objects": { "bindings": [{ "name": "COUNTER", "class_name": "Counter" }] },
          "migrations": [{ "tag": "v1", "new_sqlite_classes": ["Counter"] }],
          "triggers": { "crons": ["*/5 * * * *"] },
          "routes": [{ "pattern": "api.example.com/*", "zone_name": "example.com" }, { "pattern": "edge.example.com", "custom_domain": true }],
        }"#;
        let objs = render_with(config, "wrangler.jsonc", json!({ "bundle": format!("sha256:{HEX}") }))?;
        let w = objs.first().ok_or("no worker")?;
        assert_eq!(crate::obj::name(w), "edge");
        assert_eq!(w.pointer("/spec/main_module"), Some(&json!("index.js")));
        let kinds: Vec<&str> = w.pointer("/spec/bindings").and_then(Value::as_array).map(|b| b.iter().filter_map(|x| x.get("type").and_then(Value::as_str)).collect()).unwrap_or_default();
        // In name order: CACHE, COUNTER, LIMITS, MODE.
        assert_eq!(kinds, ["kv_namespace", "durable_object_namespace", "json", "plain_text"]);
        assert_eq!(w.pointer("/spec/crons"), Some(&json!(["*/5 * * * *"])));
        assert_eq!(w.pointer("/spec/migrations/0/tag"), Some(&json!("v1")));
        let routes: Vec<(&str, &str)> = objs.iter().skip(1).map(|o| (crate::obj::kind(o), crate::obj::name(o))).collect();
        assert_eq!(routes, [("Route", "api.example.com/*"), ("CustomDomain", "edge.example.com")]);
        assert_eq!(objs.get(1).and_then(|r| r.pointer("/spec/zone_name")), Some(&json!("example.com")));
        assert_eq!(files(&json!({})), ["wrangler.json", "wrangler.jsonc", "wrangler.toml"]);
        // A bundle that isn't an artifact, and a config without a date, are refused.
        assert!(render_with(config, "wrangler.jsonc", json!({ "bundle": "dist/" })).is_err());
        assert!(render_with(r#"{"name":"x"}"#, "wrangler.json", json!({ "bundle": format!("sha256:{HEX}") })).is_err_and(|e| e.contains("compatibility_date")));
        Ok(())
    }

    #[test]
    fn environments_inherit_what_wrangler_inherits() -> Result<(), String> {
        let config = r#"
name = "edge"
main = "src/worker.ts"
compatibility_date = "2026-09-01"
[vars]
MODE = "dev"
[env.staging]
[env.staging.vars]
MODE = "staging"
"#;
        let objs = render_with(config, "wrangler.toml", json!({ "bundle": format!("sha256:{HEX}"), "env": "staging" }))?;
        let w = objs.first().ok_or("no worker")?;
        assert_eq!(crate::obj::name(w), "edge-staging");
        assert_eq!(w.pointer("/spec/main_module"), Some(&json!("worker.js")));
        assert_eq!(w.pointer("/spec/bindings/0/text"), Some(&json!("staging")), "an environment's own vars");
        assert_eq!(w.pointer("/spec/compatibility_date"), Some(&json!("2026-09-01")));
        Ok(())
    }

    /// A migration that deletes a class deletes its data: only when the definition says so.
    #[test]
    fn deleting_a_durable_object_class_needs_confirming() -> Result<(), String> {
        let config = r#"{ "name": "edge", "compatibility_date": "2026-09-01",
          "migrations": [{ "tag": "v1", "new_classes": ["Old"] }, { "tag": "v2", "deleted_classes": ["Old"] }] }"#;
        let refused = render_with(config, "wrangler.json", json!({ "bundle": format!("sha256:{HEX}") }));
        assert!(refused.is_err_and(|e| e.contains("deletes the Durable Object class Old")));
        let objs = render_with(config, "wrangler.json", json!({ "bundle": format!("sha256:{HEX}"), "confirm-delete": ["Old"] }))?;
        let migrations = objs.first().and_then(|w| w.pointer("/spec/migrations")).and_then(Value::as_array).cloned().unwrap_or_default();
        // The steps after the tag the Worker has.
        assert_eq!(steps_after(&migrations, Some("v1"))?, Some((vec![json!({ "deleted_classes": ["Old"] })], "v2".to_owned())));
        assert_eq!(steps_after(&migrations, Some("v2"))?, None);
        assert_eq!(steps_after(&migrations, None)?.map(|(s, t)| (s.len(), t)), Some((2, "v2".to_owned())));
        assert!(steps_after(&migrations, Some("v9")).is_err());
        Ok(())
    }

    #[test]
    fn health_reads_the_deployment() {
        let w = |versions: Value| json!({ "apiVersion": format!("{GROUP}/v1"), "kind": "Worker", "status": { "deployment": { "versions": versions } } });
        assert_eq!(health(&w(json!([{ "version_id": "a", "percentage": 100 }]))).map(|s| s.health), Some(Health::Healthy));
        assert_eq!(health(&w(json!([{ "version_id": "a", "percentage": 90 }, { "version_id": "b", "percentage": 10 }]))).map(|s| s.health), Some(Health::Progressing));
        assert_eq!(module_type("index.js"), "application/javascript+module");
        assert_eq!(module_type("lib.wasm"), "application/wasm");
    }
}
