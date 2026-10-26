//! `fly` (docs/cd-targets.md §3): a Fly.io app's machines, each release rolled onto them one at a
//! time, as flyctl's rolling strategy does.
//!
//! ```yaml
//! web:
//!   fly:
//!     app: shop-web                                     # made if it isn't there
//!     image: "registry.fly.io/shop-web@${{ release.artifacts.image.digest }}"
//!     config: fly.toml                                  # fly.toml (found by default), or the machines' config
//!     count: 2                                          # how many machines (1)
//!     region: ams                                       # for machines kiln makes (fly.toml's primary_region)
//! ```
//!
//! - **App** (`shop-web`): `spec.config` is the config each machine runs, as the Machines API
//!   spells it (`image`, `env`, `services`, `guest`, `checks`), `spec.count` how many, and
//!   `spec.region` where kiln makes them. fly.toml is read into that: its `[env]`,
//!   `[http_service]` (and its checks), `[[services]]`, `[[vm]]`, `[checks]`, `primary_region`
//!   and `[build] image`.
//! - **Rollouts.** A release updates one machine at a time, under a lease, and goes on once it
//!   started and its checks pass; one that doesn't stops the rollout, the machines after it on
//!   the release before. A rollback rolls the earlier config out the same way.
//! - **Health** is the machines': started with their checks passing, starting, or one that exited
//!   or whose checks fail.

use serde_json::{Map, Value, json};

use super::{RenderInput, apply_sets, file_settings, object, status, text};
use crate::health::Status;
use crate::model::Health;

pub const GROUP: &str = "fly.kiln.dev";

/// Where the app's config is when the definition names none.
pub const DEFAULT_CONFIG: &str = "fly.toml";

/// The files a render reads.
pub fn files(spec: &Value) -> Vec<String> {
    let mut f = file_settings(spec, &["config"]);
    if spec.get("config").is_none() {
        f.push(DEFAULT_CONFIG.to_owned());
    }
    f
}

fn int(v: &toml::Value) -> Option<i64> {
    v.as_integer().or_else(|| v.as_str().and_then(|s| s.parse().ok()))
}

/// A memory size as fly.toml writes it (`1gb`, `512mb`, `1024`) in megabytes.
fn megabytes(v: &toml::Value) -> Option<i64> {
    if let Some(n) = v.as_integer() {
        return Some(n);
    }
    let s = v.as_str()?.trim().to_ascii_lowercase();
    if let Some(g) = s.strip_suffix("gb") {
        return g.trim().parse::<f64>().ok().map(|g| (g * 1024.0) as i64);
    }
    s.trim_end_matches("mb").trim().parse().ok()
}

fn json_of(v: &toml::Value) -> Value {
    serde_json::to_value(v).unwrap_or(Value::Null)
}

/// fly.toml as the machines' config, and its primary region and image.
pub fn from_fly_toml(text: &str) -> Result<(Value, Option<String>, Option<String>), String> {
    let t: toml::Value = toml::from_str(text).map_err(|e| format!("fly.toml: {e}"))?;
    let mut config = Map::new();
    if let Some(env) = t.get("env").and_then(toml::Value::as_table) {
        config.insert("env".into(), Value::Object(env.iter().map(|(k, v)| (k.clone(), json!(v.as_str().map_or_else(|| v.to_string(), str::to_owned)))).collect()));
    }
    let mut services = Vec::new();
    if let Some(h) = t.get("http_service").and_then(toml::Value::as_table) {
        let mut svc = Map::new();
        svc.insert("protocol".into(), json!("tcp"));
        svc.insert("internal_port".into(), json!(h.get("internal_port").and_then(int).unwrap_or(8080)));
        let force = h.get("force_https").and_then(toml::Value::as_bool).unwrap_or(false);
        svc.insert("ports".into(), json!([{ "port": 80, "handlers": ["http"], "force_https": force }, { "port": 443, "handlers": ["tls", "http"] }]));
        for (k, to) in [("auto_stop_machines", "autostop"), ("auto_start_machines", "autostart"), ("min_machines_running", "min_machines_running")] {
            if let Some(v) = h.get(k) {
                svc.insert(to.into(), json_of(v));
            }
        }
        if let Some(checks) = h.get("checks").and_then(toml::Value::as_array) {
            svc.insert("checks".into(), Value::Array(checks.iter().map(|c| { let mut c = json_of(c); if let Some(m) = c.as_object_mut() { m.entry("type").or_insert(json!("http")); } c }).collect()));
        }
        services.push(Value::Object(svc));
    }
    for s in t.get("services").and_then(toml::Value::as_array).into_iter().flatten() {
        let mut svc = json_of(s);
        if let Some(m) = svc.as_object_mut() {
            let mut checks = Vec::new();
            for (k, kind) in [("tcp_checks", "tcp"), ("http_checks", "http")] {
                for c in m.remove(k).and_then(|v| v.as_array().cloned()).unwrap_or_default() {
                    let mut c = c;
                    if let Some(cm) = c.as_object_mut() {
                        cm.insert("type".into(), json!(kind));
                    }
                    checks.push(c);
                }
            }
            if !checks.is_empty() {
                m.insert("checks".into(), Value::Array(checks));
            }
        }
        services.push(svc);
    }
    if !services.is_empty() {
        config.insert("services".into(), Value::Array(services));
    }
    if let Some(vm) = t.get("vm").and_then(toml::Value::as_array).and_then(|v| v.first()).and_then(toml::Value::as_table) {
        let mut guest = Map::new();
        if let Some(m) = vm.get("memory").and_then(megabytes).or_else(|| vm.get("memory_mb").and_then(int)) {
            guest.insert("memory_mb".into(), json!(m));
        }
        if let Some(c) = vm.get("cpus").and_then(int) {
            guest.insert("cpus".into(), json!(c));
        }
        if let Some(k) = vm.get("cpu_kind").and_then(toml::Value::as_str) {
            guest.insert("cpu_kind".into(), json!(k));
        }
        config.insert("guest".into(), Value::Object(guest));
    }
    if let Some(c) = t.get("checks").and_then(toml::Value::as_table) {
        config.insert("checks".into(), json_of(&toml::Value::Table(c.clone())));
    }
    let region = t.get("primary_region").and_then(toml::Value::as_str).map(str::to_owned);
    let image = t.get("build").and_then(|b| b.get("image")).and_then(toml::Value::as_str).map(str::to_owned);
    Ok((Value::Object(config), region, image))
}

/// An app's objects.
pub fn render(input: &RenderInput<'_>) -> Result<Vec<Value>, String> {
    let app = text(input.spec, "app").ok_or("fly needs app: the app's name")?;
    let (mut config, mut region, mut image) = (json!({}), None, None);
    match input.spec.get("config") {
        Some(Value::String(path)) if path.ends_with(".toml") => {
            let bytes = (input.read)(path).ok_or_else(|| format!("config: no file {path} in the source"))?;
            (config, region, image) = from_fly_toml(&String::from_utf8_lossy(&bytes))?;
        }
        Some(_) => config = super::file_or_object(input, "config")?.unwrap_or_else(|| json!({})),
        None => {
            if let Some(bytes) = (input.read)(DEFAULT_CONFIG) {
                (config, region, image) = from_fly_toml(&String::from_utf8_lossy(&bytes))?;
            }
        }
    }
    let image = text(input.spec, "image").map(str::to_owned).or(image).ok_or("fly needs image: the image the machines run (${{ release.artifacts.<name>.digest }})")?;
    let m = config.as_object_mut().ok_or("config: the machines' config")?;
    m.insert("image".into(), json!(image));
    if let Some(r) = text(input.spec, "region") {
        region = Some(r.to_owned());
    }
    let count = match input.spec.get("count") {
        None | Some(Value::Null) => 1,
        Some(v) => v.as_u64().or_else(|| v.as_str().and_then(|s| s.parse().ok())).filter(|n| *n >= 1 && *n <= 100).ok_or("count: 1 to 100 machines")?,
    };
    let mut spec = json!({ "config": config, "count": count });
    if let (Some(r), Value::Object(s)) = (region, &mut spec) {
        s.insert("region".into(), json!(r));
    }
    apply_sets(&mut spec, input.spec.get("set"))?;
    Ok(vec![object(GROUP, "App", app, None, spec)])
}

/// A machine's state, as the object's status says it.
fn machine_state(m: &Value) -> Value {
    let exit = m.get("events").and_then(Value::as_array).into_iter().flatten().find(|e| e.get("type").and_then(Value::as_str) == Some("exit")).and_then(|e| e.pointer("/request/exit_event/exit_code")).cloned();
    let checks: Vec<Value> = m.get("checks").and_then(Value::as_array).into_iter().flatten().map(|c| json!({ "name": c.get("name"), "status": c.get("status") })).collect();
    json!({ "id": m.get("id"), "state": m.get("state"), "region": m.get("region"), "image": m.pointer("/config/image"), "exit": exit, "checks": checks })
}

/// An App object from Fly's machines (destroyed ones aside): the first machine's config, how many
/// run, and where kiln makes them (`applied`'s, while they're all there).
pub fn live_app(app: &str, machines: &[Value], applied: Option<&Value>) -> Option<Value> {
    let config = machines.first().and_then(|m| m.get("config")).cloned().unwrap_or_else(|| json!({}));
    let mut spec = json!({ "config": config, "count": machines.len() });
    if let (Some(r), Value::Object(s)) = (applied.and_then(|a| a.pointer("/spec/region")).and_then(Value::as_str), &mut spec)
        && machines.iter().all(|m| m.get("region").and_then(Value::as_str) == Some(r))
    {
        s.insert("region".into(), json!(r));
    }
    let mut o = object(GROUP, "App", app, None, spec);
    if let Value::Object(m) = &mut o {
        m.insert("status".into(), json!({ "machines": machines.iter().map(machine_state).collect::<Vec<_>>() }));
    }
    Some(o)
}

pub fn health(o: &Value) -> Option<Status> {
    if crate::obj::kind(o) != "App" {
        return None;
    }
    let ms = o.pointer("/status/machines").and_then(Value::as_array)?;
    if ms.is_empty() {
        return status(Health::Progressing, "no machines yet");
    }
    let s = |m: &Value, k: &str| m.get(k).and_then(Value::as_str).unwrap_or_default().to_owned();
    for m in ms {
        if let Some(code) = m.get("exit").and_then(Value::as_i64).filter(|c| *c != 0)
            && s(m, "state") != "started"
        {
            return status(Health::Degraded, format!("machine {} exited with code {code}", s(m, "id")));
        }
        if let Some(c) = m.get("checks").and_then(Value::as_array).into_iter().flatten().find(|c| c.get("status").and_then(Value::as_str) == Some("critical")) {
            return status(Health::Degraded, format!("machine {}'s check {} fails", s(m, "id"), s(c, "name")));
        }
    }
    if let Some(m) = ms.iter().find(|m| matches!(s(m, "state").as_str(), "created" | "starting" | "replacing" | "updating")) {
        return status(Health::Progressing, format!("machine {} is {}", s(m, "id"), s(m, "state")));
    }
    status(Health::Healthy, "")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fly_tomls_read_as_machine_config() -> Result<(), String> {
        let text = "app = 'shop-web'\nprimary_region = 'ams'\n[build]\nimage = 'registry.fly.io/shop-web:old'\n[env]\nMODE = 'prod'\nPORT = 8080\n[http_service]\ninternal_port = 8080\nforce_https = true\nauto_stop_machines = 'stop'\n[[http_service.checks]]\npath = '/healthz'\ninterval = '15s'\n[[vm]]\nmemory = '1gb'\ncpus = 2\n";
        let (config, region, image) = from_fly_toml(text)?;
        assert_eq!((region.as_deref(), image.as_deref()), (Some("ams"), Some("registry.fly.io/shop-web:old")));
        assert_eq!(config.get("env"), Some(&json!({ "MODE": "prod", "PORT": "8080" })));
        assert_eq!(config.pointer("/services/0/internal_port"), Some(&json!(8080)));
        assert_eq!(config.pointer("/services/0/ports/0/force_https"), Some(&json!(true)));
        assert_eq!(config.pointer("/services/0/autostop"), Some(&json!("stop")));
        assert_eq!(config.pointer("/services/0/checks/0"), Some(&json!({ "path": "/healthz", "interval": "15s", "type": "http" })));
        assert_eq!(config.get("guest"), Some(&json!({ "memory_mb": 1024, "cpus": 2 })));
        Ok(())
    }
}
