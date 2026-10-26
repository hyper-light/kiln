//! A fake of Azure for tests (Resource Manager's container apps, sites, scale sets, static web
//! apps and storage accounts; an app's SCM site; blob storage), in memory, behaving as Azure
//! documents it where kiln depends on it: changes are long-running operations (`Azure-AsyncOperation`
//! or `Location`); a revision provisions before traffic serves from it, and a revision suffix
//! named again with other content is refused; a slot swap trades what two slots run; a zip deploy
//! is a deployment the SCM site records; a scale set's rolling upgrade moves an instance a read,
//! and stops (Faulted) at one that fails; a static web app deploys from a URL. Resource Manager's
//! requests carry the token; blobs are read and written with a SAS instead.

use std::collections::BTreeMap;
use std::sync::Arc;

use parking_lot::Mutex;
use serde_json::{Map, Value, json};

use crate::http::server::{self as srv, Request, Response};

/// The token the fake takes.
pub const TOKEN: &str = "azure-test-token";

/// Tests on the fake run one at a time: the endpoints they point at it are the process's.
pub static AZURE: Mutex<()> = Mutex::new(());

/// A container app's revision as the fake keeps it.
#[derive(Clone, Debug, Default)]
pub struct Revision {
    pub template: Value,
    pub active: bool,
    /// Reads left before it's provisioned, or failed to be.
    pub busy: u32,
    pub failed: Option<String>,
}

/// A container app as the fake keeps it.
#[derive(Clone, Debug, Default)]
pub struct ContainerApp {
    pub body: Value,
    pub revisions: BTreeMap<String, Revision>,
    /// Its revisions, oldest first.
    pub order: Vec<String>,
    /// The revision its last update made, if it made one.
    pub last: Option<String>,
}

/// A site (an app, or one of its slots) as the fake keeps it: what it runs is its config,
/// settings and current deployment, which a swap trades.
#[derive(Clone, Debug, Default)]
pub struct Site {
    pub kind: String,
    pub config: Map<String, Value>,
    pub settings: Map<String, Value>,
    pub availability: String,
    /// Its SCM site's deployments, newest last.
    pub deployments: Vec<Value>,
}

/// A scale set as the fake keeps it: its instances (the image each runs), and its rolling upgrade.
#[derive(Clone, Debug, Default)]
pub struct ScaleSet {
    pub body: Value,
    pub instances: Vec<Value>,
    pub upgrade: Option<Value>,
}

/// A static web app as the fake keeps it: its production build (status, when).
#[derive(Clone, Debug, Default)]
pub struct StaticSite {
    pub body: Value,
    pub build: Option<(String, String)>,
}

/// A long-running operation: what finishes it, polls left, and why it failed.
#[derive(Clone, Debug, Default)]
pub struct Op {
    pub about: String,
    pub polls: u32,
    pub error: Option<String>,
    /// Polled at its `Location` (202 until done), or as an `Azure-AsyncOperation` (a status).
    pub location: bool,
}

#[derive(Default)]
pub struct AzureState {
    pub base: String,
    pub apps: BTreeMap<String, ContainerApp>,
    /// Sites by app and slot (`` for production).
    pub sites: BTreeMap<(String, String), Site>,
    pub scale_sets: BTreeMap<String, ScaleSet>,
    pub static_sites: BTreeMap<String, StaticSite>,
    pub accounts: BTreeMap<String, Vec<String>>,
    /// Blobs' bytes, by account and name.
    pub blobs: BTreeMap<(String, String), Vec<u8>>,
    pub ops: BTreeMap<String, Op>,
    /// Every change, in order: `PutContainerApp shop shop--k…`.
    pub calls: Vec<String>,
    pub next: u64,
}

#[derive(Default)]
pub struct FakeAzure {
    pub state: Mutex<AzureState>,
}

fn err(status: u16, code: &str, message: &str) -> Response {
    srv::json(http::StatusCode::from_u16(status).unwrap_or(http::StatusCode::BAD_REQUEST), &json!({ "error": { "code": code, "message": message } }))
}

fn ok(v: Value) -> Response {
    srv::json(http::StatusCode::OK, &v)
}

/// Sets `v` at `path`, making the objects on the way.
fn put(v: &mut Value, path: &[&str], x: Value) {
    if let Some((last, parents)) = path.split_last()
        && let Some(m) = kiln_cd::obj::ensure_map(v, parents)
    {
        m.insert((*last).to_owned(), x);
    }
}

fn next(st: &mut AzureState) -> u64 {
    st.next += 1;
    st.next
}

/// An answer with an operation to follow: `Azure-AsyncOperation` (a status to read), or
/// `Location` (202 until done).
fn with_op(st: &mut AzureState, status: u16, body: Value, about: &str, polls: u32, location: bool) -> Response {
    let id = format!("op{}", next(st));
    st.ops.insert(id.clone(), Op { about: about.to_owned(), polls, error: None, location });
    let url = format!("{}/operations/{id}", st.base);
    let r = srv::json(http::StatusCode::from_u16(status).unwrap_or(http::StatusCode::OK), &body);
    let name = http::header::HeaderName::from_static(if location { "location" } else { "azure-asyncoperation" });
    srv::with_header(r, name, &url)
}

fn query_of(q: Option<&str>) -> BTreeMap<String, String> {
    q.unwrap_or_default().split('&').filter_map(|kv| kv.split_once('=')).map(|(k, v)| (k.to_owned(), srv::decode(v))).collect()
}

impl srv::Handler for FakeAzure {
    async fn handle(self: Arc<Self>, req: Request) -> Response {
        let (method, path, query) = (req.method().clone(), req.uri().path().to_owned(), query_of(req.uri().query()));
        let auth = srv::header_str(&req, &http::header::AUTHORIZATION).unwrap_or("").to_owned();
        let raw = srv::body_bytes(req.into_body(), 64 << 20).await.unwrap_or_default();
        let mut st = self.state.lock();
        let parts: Vec<String> = path.trim_start_matches('/').split('/').map(str::to_owned).collect();
        let first = parts.first().cloned().unwrap_or_default();
        // A blob, with a SAS and no token.
        if let Some(account) = first.strip_suffix(".blob.core.windows.net") {
            if !query.contains_key("sig") || !auth.is_empty() {
                return err(403, "AuthenticationFailed", "a blob is read and written with a SAS");
            }
            let blob = parts.get(2..).map(|p| p.join("/")).unwrap_or_default();
            if parts.get(1).map(String::as_str) != Some("kiln") || !st.accounts.get(account).is_some_and(|c| c.iter().any(|c| c == "kiln")) {
                return err(404, "ContainerNotFound", "The specified container does not exist.");
            }
            let key = (account.to_owned(), blob.clone());
            return match method.as_str() {
                "HEAD" | "GET" => match st.blobs.get(&key) {
                    Some(b) if method == http::Method::GET => srv::text(http::StatusCode::OK, &String::from_utf8_lossy(b)),
                    Some(_) => srv::status(http::StatusCode::OK),
                    None => srv::status(http::StatusCode::NOT_FOUND),
                },
                "PUT" => {
                    st.calls.push(format!("PutBlob {account}/{blob}"));
                    st.blobs.insert(key, raw.to_vec());
                    srv::status(http::StatusCode::CREATED)
                }
                _ => err(405, "UnsupportedHttpVerb", "no"),
            };
        }
        if auth != format!("Bearer {TOKEN}") {
            return err(401, "InvalidAuthenticationToken", "The access token is missing or invalid.");
        }
        // An app's SCM site.
        if first.ends_with(".scm.azurewebsites.net") {
            return scm(&mut st, &method, &first, parts.get(1..).unwrap_or_default(), &raw);
        }
        if first == "operations" {
            return operation(&mut st, parts.get(1).map(String::as_str).unwrap_or_default());
        }
        let body: Value = if raw.is_empty() { Value::Null } else { serde_json::from_slice(&raw).unwrap_or(Value::Null) };
        let p: Vec<&str> = parts.iter().map(String::as_str).collect();
        match p.as_slice() {
            ["subscriptions", _, "resourceGroups", rg] if method == http::Method::GET => ok(json!({ "name": rg, "location": "westeurope" })),
            ["subscriptions", _, "resourceGroups", _, "providers", "Microsoft.App", "containerApps", rest @ ..] => container_apps(&mut st, &method, rest, &body),
            ["subscriptions", _, "resourceGroups", _, "providers", "Microsoft.Web", "sites", rest @ ..] => sites(&mut st, &method, rest, &body),
            ["subscriptions", _, "resourceGroups", _, "providers", "Microsoft.Compute", "virtualMachineScaleSets", rest @ ..] => scale_sets(&mut st, &method, rest, &body),
            ["subscriptions", _, "resourceGroups", _, "providers", "Microsoft.Web", "staticSites", rest @ ..] => static_sites(&mut st, &method, rest, &body),
            ["subscriptions", _, "resourceGroups", _, "providers", "Microsoft.Storage", "storageAccounts", rest @ ..] => storage(&mut st, &method, rest),
            _ => err(404, "ResourceNotFound", &format!("the fake has no {method} {path}")),
        }
    }
}

/// An operation, moving what it's about along first.
fn operation(st: &mut AzureState, id: &str) -> Response {
    let Some(mut op) = st.ops.get(id).cloned() else { return err(404, "NotFound", "no such operation") };
    op.polls = op.polls.saturating_sub(1);
    let mut busy = op.polls > 0;
    if let Some(app) = op.about.strip_prefix("app/") {
        if let Some(a) = st.apps.get_mut(app) {
            step_app(a);
            busy |= a.revisions.values().any(|r| r.busy > 0);
            if !busy {
                op.error = app_failure(a);
            }
        }
    } else if let Some(s) = op.about.strip_prefix("vmss/")
        && let Some(ss) = st.scale_sets.get_mut(s)
    {
        put(&mut ss.body, &["properties", "provisioningState"], json!("Succeeded"));
    }
    st.ops.insert(id.to_owned(), op.clone());
    if op.location {
        return if busy { srv::status(http::StatusCode::ACCEPTED) } else { ok(json!({})) };
    }
    let status = if busy { "InProgress" } else if op.error.is_some() { "Failed" } else { "Succeeded" };
    let mut v = json!({ "status": status });
    if let (Some(e), false) = (&op.error, busy) {
        put(&mut v, &["error"], json!({ "code": "Failed", "message": e }));
    }
    ok(v)
}

// ------------------------------------------------------------------ container apps

const ACA: &str = "/subscriptions/sub/resourceGroups/rg/providers/Microsoft.App/containerApps";

/// A template with Container Apps' defaults filled in, as it keeps them.
fn aca_defaulted(t: &Value) -> Value {
    let mut t = t.clone();
    if let Some(Value::Array(cs)) = t.get_mut("containers") {
        for c in cs.iter_mut().filter_map(Value::as_object_mut) {
            c.entry("resources").or_insert(json!({ "cpu": 0.5, "memory": "1Gi", "ephemeralStorage": "2Gi" }));
        }
    }
    if let Some(m) = t.as_object_mut() {
        m.entry("scale").or_insert(json!({ "minReplicas": 0, "maxReplicas": 10 }));
    }
    t
}

fn step_app(a: &mut ContainerApp) {
    for r in a.revisions.values_mut().filter(|r| r.busy > 0) {
        r.busy -= 1;
        if r.busy == 0 && r.template.pointer("/containers/0/image").and_then(Value::as_str).unwrap_or_default().contains("broken") {
            r.failed = Some("Container 'web' was terminated with exit code '1': the image doesn't start".into());
        }
    }
    let busy = a.revisions.values().any(|r| r.busy > 0);
    let state = if busy { "InProgress" } else if app_failure(a).is_some() { "Failed" } else { "Succeeded" };
    put(&mut a.body, &["properties", "provisioningState"], json!(state));
}

/// Why an app's last update failed: the revision it made failed to provision.
fn app_failure(a: &ContainerApp) -> Option<String> {
    let made = a.last.as_ref()?;
    a.revisions.get(made)?.failed.clone().map(|f| format!("Failed to provision revision for container app: {f}"))
}

fn revision_answer(app: &str, name: &str, r: &Revision) -> Value {
    let (prov, running, health) = match (r.busy, &r.failed) {
        (1.., _) => ("Provisioning", "Processing", "None"),
        (0, Some(_)) => ("Failed", "Failed", "Unhealthy"),
        (0, None) if r.active => ("Provisioned", "Running", "Healthy"),
        (0, None) => ("Provisioned", "Stopped", "None"),
    };
    json!({ "name": name, "id": format!("{ACA}/{app}/revisions/{name}"), "properties": { "active": r.active, "provisioningState": prov, "runningState": running, "healthState": health, "replicas": if r.active { 1 } else { 0 }, "template": r.template, "provisioningError": r.failed } })
}

fn app_answer(name: &str, a: &ContainerApp) -> Value {
    let mut v = a.body.clone();
    let latest = a.order.last().cloned().unwrap_or_default();
    let ready = a.order.iter().rev().find(|r| a.revisions.get(*r).is_some_and(|x| x.busy == 0 && x.failed.is_none())).cloned().unwrap_or_default();
    put(&mut v, &["id"], json!(format!("{ACA}/{name}")));
    put(&mut v, &["name"], json!(name));
    put(&mut v, &["type"], json!("Microsoft.App/containerApps"));
    put(&mut v, &["properties", "latestRevisionName"], json!(latest));
    put(&mut v, &["properties", "latestReadyRevisionName"], json!(ready));
    if let Some(i) = v.pointer_mut("/properties/configuration/ingress").and_then(Value::as_object_mut) {
        i.insert("fqdn".into(), json!(format!("{name}.happyhill-1234.westeurope.azurecontainerapps.io")));
        i.entry("transport").or_insert(json!("Auto"));
    }
    v
}

fn container_apps(st: &mut AzureState, method: &http::Method, rest: &[&str], body: &Value) -> Response {
    match (method.as_str(), rest) {
        ("GET", []) => ok(json!({ "value": st.apps.iter().map(|(n, a)| app_answer(n, a)).collect::<Vec<_>>() })),
        ("GET", [name]) => match st.apps.get_mut(*name) {
            Some(a) => {
                step_app(a);
                ok(app_answer(name, a))
            }
            None => err(404, "ResourceNotFound", &format!("The Resource 'Microsoft.App/containerApps/{name}' was not found.")),
        },
        ("PUT", [name]) => {
            let mut a = st.apps.get(*name).cloned().unwrap_or_default();
            let Some(t) = body.pointer("/properties/template") else { return err(400, "InvalidRequestContent", "a template is required") };
            let template = aca_defaulted(t);
            let current = a.body.pointer("/properties/template").cloned().unwrap_or(Value::Null);
            let strip = |t: &Value| {
                let mut t = t.clone();
                if let Some(m) = t.as_object_mut() {
                    m.remove("revisionSuffix");
                }
                t
            };
            let changed = strip(&template) != strip(&current) || template.get("revisionSuffix") != current.get("revisionSuffix");
            let mut made = None;
            if changed {
                let sfx = template.get("revisionSuffix").and_then(Value::as_str).map_or_else(|| format!("r{}", st.next + 1), str::to_owned);
                let rev = format!("{name}--{sfx}");
                if a.revisions.contains_key(&rev) {
                    return err(400, "ContainerAppRevisionSuffixInUse", &format!("Revision with suffix {sfx} already exists"));
                }
                made = Some(rev);
            }
            let traffic = body.pointer("/properties/configuration/ingress/traffic").and_then(Value::as_array).cloned().unwrap_or_default();
            if !traffic.is_empty() && traffic.iter().map(|t| t.get("weight").and_then(Value::as_u64).unwrap_or(0)).sum::<u64>() != 100 {
                return err(400, "ContainerAppInvalidTrafficWeight", "the traffic's weights must add up to 100");
            }
            for t in &traffic {
                if let Some(r) = t.get("revisionName").and_then(Value::as_str)
                    && !a.revisions.contains_key(r)
                    && made.as_deref() != Some(r)
                {
                    return err(400, "ContainerAppRevisionNotFound", &format!("Revision {r} not found"));
                }
            }
            if body.pointer("/properties/configuration/activeRevisionsMode").and_then(Value::as_str) != Some("Multiple") {
                return err(400, "InvalidRequestContent", "the fake runs apps in Multiple revision mode");
            }
            let mut stored = body.clone();
            put(&mut stored, &["properties", "template"], if made.is_some() { template.clone() } else { current });
            if let Some(rev) = &made {
                a.revisions.insert(rev.clone(), Revision { template, active: true, busy: 2, failed: None });
                a.order.push(rev.clone());
            }
            a.last.clone_from(&made);
            a.body = stored;
            let existed = st.apps.contains_key(*name);
            let moved: Vec<String> = traffic.iter().map(|t| format!("{}:{}", t.get("revisionName").and_then(Value::as_str).unwrap_or("latest"), t.get("weight").and_then(Value::as_u64).unwrap_or(0))).collect();
            st.calls.push(format!("PutContainerApp {name}{}{}", made.map(|m| format!(" revision={m}")).unwrap_or_default(), if moved.is_empty() { String::new() } else { format!(" traffic={}", moved.join(",")) }));
            step_app(&mut a);
            let answer = app_answer(name, &a);
            st.apps.insert((*name).to_owned(), a);
            with_op(st, if existed { 200 } else { 201 }, answer, &format!("app/{name}"), 2, false)
        }
        ("DELETE", [name]) => match st.apps.remove(*name) {
            Some(_) => {
                st.calls.push(format!("DeleteContainerApp {name}"));
                with_op(st, 202, Value::Null, "", 1, true)
            }
            None => srv::status(http::StatusCode::NO_CONTENT),
        },
        ("GET", [name, "revisions"]) => match st.apps.get(*name) {
            Some(a) => ok(json!({ "value": a.order.iter().filter_map(|r| a.revisions.get(r).map(|x| revision_answer(name, r, x))).collect::<Vec<_>>() })),
            None => err(404, "ResourceNotFound", "no such app"),
        },
        ("GET", [name, "revisions", rev]) => match st.apps.get_mut(*name) {
            Some(a) => {
                step_app(a);
                match a.revisions.get(*rev) {
                    Some(r) => ok(revision_answer(name, rev, r)),
                    None => err(404, "ContainerAppRevisionNotFound", &format!("Revision {rev} not found")),
                }
            }
            None => err(404, "ResourceNotFound", "no such app"),
        },
        ("POST", [name, "revisions", rev, action @ ("activate" | "deactivate")]) => {
            let Some(r) = st.apps.get_mut(*name).and_then(|a| a.revisions.get_mut(*rev)) else { return err(404, "ContainerAppRevisionNotFound", "no such revision") };
            r.active = *action == "activate";
            st.calls.push(format!("{} {rev}", if *action == "activate" { "ActivateRevision" } else { "DeactivateRevision" }));
            ok(Value::Null)
        }
        _ => err(404, "ResourceNotFound", "the fake has no such container apps call"),
    }
}

// ------------------------------------------------------------------ sites

fn scm_host(app: &str, slot: &str) -> String {
    if slot.is_empty() { format!("{app}.scm.azurewebsites.net") } else { format!("{app}-{slot}.scm.azurewebsites.net") }
}

fn site_answer(app: &str, slot: &str, s: &Site) -> Value {
    let host = if slot.is_empty() { format!("{app}.azurewebsites.net") } else { format!("{app}-{slot}.azurewebsites.net") };
    let name = if slot.is_empty() { app.to_owned() } else { format!("{app}/{slot}") };
    json!({ "name": name, "kind": s.kind, "properties": { "state": "Running", "availabilityState": s.availability, "defaultHostName": host,
        "hostNameSslStates": [{ "name": host, "hostType": "Standard" }, { "name": scm_host(app, slot), "hostType": "Repository" }] } })
}

fn sites(st: &mut AzureState, method: &http::Method, rest: &[&str], body: &Value) -> Response {
    let (app, slot, tail): (&str, &str, &[&str]) = match rest {
        [app, "slots", slot, tail @ ..] if !matches!(tail.first(), Some(&"slotsswap")) => (app, slot, tail),
        [app, tail @ ..] => (app, "", tail),
        [] => return ok(json!({ "value": st.sites.iter().filter(|((_, s), _)| s.is_empty()).map(|((a, s), x)| site_answer(a, s, x)).collect::<Vec<_>>() })),
    };
    let key = (app.to_owned(), slot.to_owned());
    if !st.sites.contains_key(&key) {
        return err(404, "ResourceNotFound", &format!("The Resource 'Microsoft.Web/sites/{app}' was not found."));
    }
    match (method.as_str(), tail) {
        ("GET", []) => ok(st.sites.get(&key).map(|s| site_answer(app, slot, s)).unwrap_or_default()),
        ("GET", ["config", "web"]) => ok(json!({ "properties": st.sites.get(&key).map(|s| s.config.clone()).unwrap_or_default() })),
        ("PATCH", ["config", "web"]) => {
            let Some(s) = st.sites.get_mut(&key) else { return err(404, "ResourceNotFound", "no site") };
            for (k, v) in body.get("properties").and_then(Value::as_object).into_iter().flatten() {
                s.config.insert(k.clone(), v.clone());
            }
            let image = s.config.get("linuxFxVersion").and_then(Value::as_str).unwrap_or_default().to_owned();
            s.availability = if image.contains("broken") { "Limited".into() } else { "Normal".into() };
            st.calls.push(format!("PatchConfig {app}{} {image}", if slot.is_empty() { String::new() } else { format!("/{slot}") }));
            ok(json!({ "properties": st.sites.get(&key).map(|s| s.config.clone()).unwrap_or_default() }))
        }
        ("POST", ["config", "appsettings", "list"]) => ok(json!({ "properties": st.sites.get(&key).map(|s| s.settings.clone()).unwrap_or_default() })),
        ("PUT", ["config", "appsettings"]) => {
            if let Some(s) = st.sites.get_mut(&key) {
                s.settings = body.get("properties").and_then(Value::as_object).cloned().unwrap_or_default();
            }
            st.calls.push(format!("PutSettings {app}{}", if slot.is_empty() { String::new() } else { format!("/{slot}") }));
            ok(body.clone())
        }
        ("POST", ["slots", s, "slotsswap"]) => {
            let other = (app.to_owned(), (*s).to_owned());
            if body.get("targetSlot").and_then(Value::as_str) != Some("production") {
                return err(400, "BadRequest", "the fake swaps into production");
            }
            let (Some(a), Some(b)) = (st.sites.get(&key).cloned(), st.sites.get(&other).cloned()) else { return err(404, "ResourceNotFound", "no slot") };
            let swap = |into: &mut Site, from: &Site| {
                into.config = from.config.clone();
                into.settings = from.settings.clone();
                into.availability = from.availability.clone();
            };
            if let Some(x) = st.sites.get_mut(&key) {
                swap(x, &b);
            }
            if let Some(x) = st.sites.get_mut(&other) {
                swap(x, &a);
            }
            st.calls.push(format!("SwapSlot {app} {s}"));
            with_op(st, 202, Value::Null, "", 2, true)
        }
        _ => err(404, "ResourceNotFound", "the fake has no such sites call"),
    }
}

/// An app's SCM site: zip deploys and their records.
fn scm(st: &mut AzureState, method: &http::Method, host: &str, rest: &[String], raw: &[u8]) -> Response {
    let Some(key) = st.sites.keys().find(|(a, s)| scm_host(a, s) == host).cloned() else { return err(404, "NotFound", "no such SCM site") };
    let base = st.base.clone();
    let rest: Vec<&str> = rest.iter().map(String::as_str).collect();
    match (method.as_str(), rest.as_slice()) {
        ("POST", ["api", "zipdeploy"]) => {
            let id = format!("d{}", next(st));
            let failed = String::from_utf8_lossy(raw).contains("broken");
            let d = json!({ "id": id, "status": if failed { 3 } else { 4 }, "status_text": if failed { "Deployment failed: the app didn't build" } else { "" }, "complete": false, "deployer": "ZipDeploy" });
            if let Some(s) = st.sites.get_mut(&key) {
                s.deployments.push(d);
            }
            st.calls.push(format!("ZipDeploy {host}"));
            let r = srv::status(http::StatusCode::ACCEPTED);
            srv::with_header(r, http::header::LOCATION, &format!("{base}/{host}/api/deployments/{id}"))
        }
        ("GET", ["api", "deployments", which]) => {
            let Some(s) = st.sites.get_mut(&key) else { return err(404, "NotFound", "no site") };
            let pos = if *which == "latest" { s.deployments.len().checked_sub(1) } else { s.deployments.iter().position(|d| d.get("id").and_then(Value::as_str) == Some(*which)) };
            let Some(d) = pos.and_then(|i| s.deployments.get_mut(i)) else { return err(404, "NotFound", "no deployment") };
            let answer = d.clone();
            put(d, &["complete"], json!(true));
            ok(answer)
        }
        _ => err(404, "NotFound", "the fake's SCM site has no such call"),
    }
}

// ------------------------------------------------------------------ scale sets

fn image_of(v: &Value) -> Value {
    v.pointer("/properties/virtualMachineProfile/storageProfile/imageReference").cloned().unwrap_or(Value::Null)
}

/// A read: one instance off the model moves onto it, or, with a broken image, fails and stops
/// the rolling upgrade.
fn roll(ss: &mut ScaleSet) {
    let model = image_of(&ss.body);
    let broken = model.to_string().contains("broken");
    let Some(up) = ss.upgrade.as_mut().filter(|u| u.pointer("/properties/runningStatus/code").and_then(Value::as_str) == Some("RollingForward")) else { return };
    match ss.instances.iter_mut().find(|i| *i != &model) {
        Some(_) if broken => {
            put(up, &["properties", "runningStatus", "code"], json!("Faulted"));
            put(up, &["properties", "error"], json!({ "code": "RollingUpgradeFailed", "message": "Instance 0 failed to start on the new image: its health probe never passed" }));
        }
        Some(i) => *i = model.clone(),
        None => put(up, &["properties", "runningStatus", "code"], json!("Completed")),
    }
    let on = ss.instances.iter().filter(|i| **i == model).count();
    put(up, &["properties", "progress"], json!({ "successfulInstanceCount": on, "pendingInstanceCount": ss.instances.len() - on }));
}

fn scale_sets(st: &mut AzureState, method: &http::Method, rest: &[&str], body: &Value) -> Response {
    match (method.as_str(), rest) {
        ("GET", []) => ok(json!({ "value": st.scale_sets.iter().map(|(n, s)| { let mut v = s.body.clone(); put(&mut v, &["name"], json!(n)); v }).collect::<Vec<_>>() })),
        ("GET", [name]) => match st.scale_sets.get(*name) {
            Some(s) => {
                let mut v = s.body.clone();
                put(&mut v, &["name"], json!(name));
                put(&mut v, &["id"], json!(format!("/subscriptions/sub/resourceGroups/rg/providers/Microsoft.Compute/virtualMachineScaleSets/{name}")));
                ok(v)
            }
            None => err(404, "ResourceNotFound", &format!("The Resource 'Microsoft.Compute/virtualMachineScaleSets/{name}' was not found.")),
        },
        ("PUT", [name]) => {
            if body.pointer("/properties/upgradePolicy/mode").and_then(Value::as_str) != Some("Rolling") {
                return err(400, "InvalidParameter", "the fake rolls upgrades");
            }
            let n = body.pointer("/sku/capacity").and_then(Value::as_u64).unwrap_or(2);
            let mut b = body.clone();
            put(&mut b, &["properties", "provisioningState"], json!("Creating"));
            let image = image_of(&b);
            st.scale_sets.insert((*name).to_owned(), ScaleSet { body: b.clone(), instances: vec![image; n as usize], upgrade: None });
            st.calls.push(format!("PutScaleSet {name}"));
            with_op(st, 201, b, &format!("vmss/{name}"), 1, false)
        }
        ("PATCH", [name]) => {
            let Some(ss) = st.scale_sets.get_mut(*name) else { return err(404, "ResourceNotFound", "no scale set") };
            let before = image_of(&ss.body);
            ss.body = kiln_cd::providers::gcp_functions::merged(&ss.body, body);
            put(&mut ss.body, &["properties", "provisioningState"], json!("Updating"));
            let after = image_of(&ss.body);
            if after != before {
                ss.upgrade = Some(json!({ "properties": { "runningStatus": { "code": "RollingForward", "startTime": "2026-10-23T00:00:00Z" }, "progress": {} } }));
            }
            let image = after.get("id").or_else(|| after.get("version")).and_then(Value::as_str).unwrap_or_default().to_owned();
            st.calls.push(format!("PatchScaleSet {name} {image}"));
            let b = ss.body.clone();
            with_op(st, 200, b, &format!("vmss/{name}"), 1, false)
        }
        ("DELETE", [name]) => match st.scale_sets.remove(*name) {
            Some(_) => {
                st.calls.push(format!("DeleteScaleSet {name}"));
                with_op(st, 202, Value::Null, "", 1, true)
            }
            None => srv::status(http::StatusCode::NO_CONTENT),
        },
        ("GET", [name, "virtualMachines"]) => match st.scale_sets.get_mut(*name) {
            Some(ss) => {
                roll(ss);
                let model = image_of(&ss.body);
                ok(json!({ "value": ss.instances.iter().enumerate().map(|(i, x)| json!({ "instanceId": i.to_string(), "properties": { "latestModelApplied": *x == model } })).collect::<Vec<_>>() }))
            }
            None => err(404, "ResourceNotFound", "no scale set"),
        },
        ("GET", [name, "rollingUpgrades", "latest"]) => match st.scale_sets.get(*name).and_then(|s| s.upgrade.clone()) {
            Some(u) => ok(u),
            None => err(404, "NotFound", "no rolling upgrade"),
        },
        _ => err(404, "ResourceNotFound", "the fake has no such scale set call"),
    }
}

// ------------------------------------------------------------------ static web apps and storage

fn static_sites(st: &mut AzureState, method: &http::Method, rest: &[&str], body: &Value) -> Response {
    match (method.as_str(), rest) {
        ("GET", []) => ok(json!({ "value": st.static_sites.iter().map(|(n, s)| { let mut v = s.body.clone(); put(&mut v, &["name"], json!(n)); v }).collect::<Vec<_>>() })),
        ("GET", [name]) => match st.static_sites.get(*name) {
            Some(s) => {
                let mut v = s.body.clone();
                put(&mut v, &["name"], json!(name));
                put(&mut v, &["properties", "defaultHostname"], json!(format!("{name}.azurestaticapps.net")));
                ok(v)
            }
            None => err(404, "ResourceNotFound", &format!("The Resource 'Microsoft.Web/staticSites/{name}' was not found.")),
        },
        ("PUT", [name]) => {
            st.static_sites.insert((*name).to_owned(), StaticSite { body: body.clone(), build: None });
            st.calls.push(format!("PutStaticSite {name}"));
            with_op(st, 201, body.clone(), "", 1, false)
        }
        ("DELETE", [name]) => match st.static_sites.remove(*name) {
            Some(_) => {
                st.calls.push(format!("DeleteStaticSite {name}"));
                with_op(st, 202, Value::Null, "", 1, true)
            }
            None => srv::status(http::StatusCode::NO_CONTENT),
        },
        ("GET", [name, "builds", "default"]) => match st.static_sites.get(*name).and_then(|s| s.build.clone()) {
            Some((status, last)) => ok(json!({ "name": "default", "properties": { "status": status, "lastUpdatedOn": last } })),
            None => err(404, "NotFound", "no build"),
        },
        ("POST", [name, "zipdeploy"]) => {
            let url = body.pointer("/properties/appZipUrl").and_then(Value::as_str).unwrap_or_default().to_owned();
            // Fetched from its URL: a blob of the fake's, read with its SAS.
            let place = url.strip_prefix(&format!("{}/", st.base)).and_then(|r| r.split_once('?')).filter(|(_, q)| q.contains("sig=")).and_then(|(p, _)| {
                let (host, blob) = p.split_once("/kiln/")?;
                Some((host.strip_suffix(".blob.core.windows.net")?.to_owned(), blob.to_owned()))
            });
            let zip = place.and_then(|k| st.blobs.get(&k).cloned());
            let n = next(st);
            let Some(s) = st.static_sites.get_mut(*name) else { return err(404, "ResourceNotFound", "no static site") };
            let status = match zip {
                None => return err(400, "BadRequest", "The zip couldn't be read from appZipUrl"),
                Some(z) if String::from_utf8_lossy(&z).contains("broken") => "Failed",
                Some(_) => "Ready",
            };
            s.build = Some((status.to_owned(), format!("2026-10-23T00:00:{:02}Z", n % 60)));
            st.calls.push(format!("ZipDeployStaticSite {name}"));
            with_op(st, 202, Value::Null, "", 2, true)
        }
        _ => err(404, "ResourceNotFound", "the fake has no such static site call"),
    }
}

fn storage(st: &mut AzureState, method: &http::Method, rest: &[&str]) -> Response {
    match (method.as_str(), rest) {
        ("GET", [account]) => match st.accounts.get(*account) {
            Some(_) => ok(json!({ "name": account, "properties": { "provisioningState": "Succeeded" } })),
            None => err(404, "ResourceNotFound", &format!("The Resource 'Microsoft.Storage/storageAccounts/{account}' was not found.")),
        },
        ("PUT", [account]) => {
            st.accounts.entry((*account).to_owned()).or_default();
            st.calls.push(format!("PutStorageAccount {account}"));
            with_op(st, 202, Value::Null, "", 1, true)
        }
        ("GET", [account, "blobServices", "default", "containers", c]) => match st.accounts.get(*account) {
            Some(cs) if cs.iter().any(|x| x == c) => ok(json!({ "name": c })),
            _ => err(404, "ContainerNotFound", "The specified container does not exist."),
        },
        ("PUT", [account, "blobServices", "default", "containers", c]) => match st.accounts.get_mut(*account) {
            Some(cs) => {
                cs.push((*c).to_owned());
                ok(json!({ "name": c }))
            }
            None => err(404, "ResourceNotFound", "no account"),
        },
        ("POST", [account, "ListAccountSas"]) if st.accounts.contains_key(*account) => ok(json!({ "accountSasToken": "sv=2023-11-03&ss=b&srt=o&sp=rcw&se=2099-01-01T00:00:00Z&sig=fake" })),
        ("POST", [account, "ListServiceSas"]) if st.accounts.contains_key(*account) => ok(json!({ "serviceSasToken": "sv=2023-11-03&sr=b&sp=r&se=2099-01-01T00:00:00Z&sig=fake-read" })),
        _ => err(404, "ResourceNotFound", "the fake has no such storage call"),
    }
}

impl AzureState {
    /// An app made with its plan, outside kiln, and slots of it.
    pub fn site(&mut self, app: &str, kind: &str, slots: &[&str]) {
        for s in std::iter::once("").chain(slots.iter().copied()) {
            self.sites.insert((app.to_owned(), s.to_owned()), Site { kind: kind.to_owned(), availability: "Normal".into(), ..Site::default() });
        }
    }

    /// Someone deploys to an app by hand: a deployment kiln didn't make.
    pub fn deploy_by_hand(&mut self, app: &str) {
        if let Some(s) = self.sites.get_mut(&(app.to_owned(), String::new())) {
            s.deployments.push(json!({ "id": "by-hand", "status": 4, "complete": true, "deployer": "Push-Deployer" }));
        }
    }
}

/// Starts the fake on a free port, and points kiln's Azure endpoints at it: its base URL.
pub async fn start(fake: Arc<FakeAzure>) -> anyhow::Result<String> {
    let tcp = hyper_rt::tcp::TcpListener::bind(std::net::SocketAddr::from(([127, 0, 0, 1], 0)), 64).map_err(|e| anyhow::anyhow!("{e}"))?;
    let addr = tcp.local_addr().map_err(|e| anyhow::anyhow!("{e}"))?;
    let base = format!("http://{addr}");
    fake.state.lock().base.clone_from(&base);
    crate::rt::spawn(async move {
        while let Ok(s) = tcp.accept().await {
            let (h, r) = (Arc::clone(&fake), s.peer_addr().ok());
            crate::rt::spawn(srv::serve_connection(crate::http::io::RtIo::new(s), h, r));
        }
    });
    crate::ci::integrations::override_endpoint("azure:management", &base);
    crate::ci::integrations::override_endpoint("azure:hosts", &base);
    Ok(base)
}

/// The test resource group's target for `provider`, and its place as an environment names it.
pub fn target(provider: &str) -> anyhow::Result<(kiln_cd::providers::Target, Value)> {
    let place = json!({ "account": "azure-test", "subscription": "sub", "resource-group": "rg" });
    let (t, _) = kiln_cd::providers::target_of(provider, place.as_object().ok_or_else(|| anyhow::anyhow!("place"))?).map_err(|e| anyhow::anyhow!("{e}"))?;
    anyhow::ensure!(t.def().is_some_and(|d| d.built), "{provider} isn't built");
    Ok((t, place))
}
