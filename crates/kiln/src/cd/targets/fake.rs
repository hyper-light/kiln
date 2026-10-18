//! A fake AWS for tests: the parts of Lambda's REST-JSON API and ECS's JSON API the providers
//! call, kept in memory, behaving as AWS documents them where kiln depends on it (a function is
//! pending once created and updating once changed; configuration can't change mid-update;
//! publishing unchanged code returns the newest version; task definitions are immutable
//! revisions; ECS fills in container defaults). Requests must be signed.

use std::collections::BTreeMap;
use std::sync::Arc;

use parking_lot::Mutex;
use serde_json::{Map, Value, json};

use crate::http::server::{self as srv, Request, Response};

#[derive(Default)]
pub struct Function {
    pub config: Map<String, Value>,
    pub code: Value,
    pub sha: String,
    /// GETs left while pending or updating.
    pub busy: u32,
    pub versions: Vec<(String, String, Value)>,
    pub aliases: BTreeMap<String, Value>,
    pub urls: BTreeMap<String, Value>,
    pub tags: Map<String, Value>,
    pub policy: Vec<String>,
}

#[derive(Default)]
pub struct State {
    pub functions: BTreeMap<String, Function>,
    /// Revisions by family; each with its status.
    pub task_defs: BTreeMap<String, Vec<Value>>,
    pub tags: BTreeMap<String, Vec<Value>>,
    pub services: BTreeMap<(String, String), Value>,
    /// S3: buckets, and their objects' bytes by key.
    pub buckets: BTreeMap<String, BTreeMap<String, Vec<u8>>>,
    /// Every operation, in order: `CreateFunction thumbs`.
    pub calls: Vec<String>,
}

#[derive(Default)]
pub struct Fake {
    pub state: Mutex<State>,
}

fn err(status: u16, code: &str, message: &str) -> Response {
    let r = srv::json(http::StatusCode::from_u16(status).unwrap_or(http::StatusCode::BAD_REQUEST), &json!({ "message": message, "__type": code }));
    srv::with_header(r, http::header::HeaderName::from_static("x-amzn-errortype"), code)
}

fn ok(v: Value) -> Response {
    srv::json(http::StatusCode::OK, &v)
}

impl srv::Handler for Fake {
    async fn handle(self: Arc<Self>, req: Request) -> Response {
        let auth = srv::header_str(&req, &http::header::AUTHORIZATION).unwrap_or("").to_owned();
        if !auth.starts_with("AWS4-HMAC-SHA256 Credential=") {
            return err(403, "UnrecognizedClientException", "unsigned");
        }
        let target = srv::header_str(&req, &http::header::HeaderName::from_static("x-amz-target")).map(str::to_owned);
        let signed_sha = srv::header_str(&req, &http::header::HeaderName::from_static("x-amz-content-sha256")).map(str::to_owned);
        let method = req.method().clone();
        let path = srv::decode(req.uri().path());
        let query = srv::query(&req);
        let raw = srv::body_bytes(req.into_body(), 4 << 20).await.unwrap_or_default();
        let mut st = self.state.lock();
        // STS's Query form, S3's buckets (kiln's are kiln-…), ECS's JSON, Lambda's REST.
        if raw.starts_with(b"Action=GetCallerIdentity") {
            return srv::text(http::StatusCode::OK, "<GetCallerIdentityResponse><GetCallerIdentityResult><Account>123456789012</Account><Arn>arn:aws:sts::123456789012:assumed-role/kiln/x</Arn></GetCallerIdentityResult></GetCallerIdentityResponse>");
        }
        if path.starts_with("/kiln-") {
            return s3(&mut st, &method, &path, &raw, signed_sha.as_deref());
        }
        let body: Value = serde_json::from_slice(&raw).unwrap_or(Value::Null);
        match target {
            Some(t) => ecs(&mut st, t.rsplit('.').next().unwrap_or_default(), &body),
            None => lambda(&mut st, &method, &path, &query, &body),
        }
    }
}

fn config_of(name: &str, f: &Function) -> Value {
    let mut c = f.config.clone();
    c.insert("FunctionName".into(), json!(name));
    c.insert("FunctionArn".into(), json!(format!("arn:aws:lambda:us-east-1:1:function:{name}")));
    c.insert("CodeSha256".into(), json!(f.sha));
    c.insert("Version".into(), json!("$LATEST"));
    let (state, update) = match (f.busy, f.versions.is_empty() && f.calls_since_create()) {
        (0, _) => ("Active", "Successful"),
        (_, true) => ("Pending", "Successful"),
        _ => ("Active", "InProgress"),
    };
    c.insert("State".into(), json!(state));
    c.insert("LastUpdateStatus".into(), json!(update));
    Value::Object(c)
}

impl Function {
    fn calls_since_create(&self) -> bool {
        self.config.get("__created").is_some()
    }
}

fn sha_of(code: &Value) -> String {
    use base64::Engine as _;
    use sha2::Digest as _;
    base64::engine::general_purpose::STANDARD.encode(sha2::Sha256::digest(code.to_string().as_bytes()))
}

const CONFIG: &[&str] = &["Role", "Handler", "Description", "Timeout", "MemorySize", "VpcConfig", "Environment", "Runtime", "Layers", "TracingConfig", "EphemeralStorage", "LoggingConfig", "PackageType", "Architectures"];

fn lambda(st: &mut State, method: &http::Method, path: &str, query: &std::collections::HashMap<String, String>, body: &Value) -> Response {
    let parts: Vec<&str> = path.trim_start_matches('/').split('/').collect();
    match (method.as_str(), parts.as_slice()) {
        ("GET", ["2015-03-31", "functions", ""]) => {
            let all: Vec<Value> = st.functions.iter().map(|(n, f)| config_of(n, f)).collect();
            ok(json!({ "Functions": all }))
        }
        ("GET", ["2015-03-31", "functions", name, "aliases"]) => match st.functions.get(*name) {
            Some(f) => ok(json!({ "Aliases": f.aliases.values().cloned().collect::<Vec<_>>() })),
            None => err(404, "ResourceNotFoundException", "Function not found"),
        },
        ("POST", ["2015-03-31", "functions"]) => {
            let name = body.get("FunctionName").and_then(Value::as_str).unwrap_or_default().to_owned();
            if st.functions.contains_key(&name) {
                return err(409, "ResourceConflictException", "Function already exist");
            }
            let mut f = Function { busy: 1, ..Function::default() };
            for k in CONFIG {
                if let Some(v) = body.get(*k) {
                    f.config.insert((*k).to_owned(), v.clone());
                }
            }
            f.config.insert("__created".into(), json!(true));
            f.code = body.get("Code").cloned().unwrap_or(Value::Null);
            f.sha = sha_of(&f.code);
            f.tags = body.get("Tags").and_then(Value::as_object).cloned().unwrap_or_default();
            st.calls.push(format!("CreateFunction {name}"));
            let c = config_of(&name, &f);
            st.functions.insert(name, f);
            ok(c)
        }
        ("GET", ["2015-03-31", "functions", name]) => match st.functions.get_mut(*name) {
            Some(f) => {
                let mut c = config_of(name, f);
                if let Some(m) = c.as_object_mut() {
                    m.remove("__created");
                }
                f.busy = f.busy.saturating_sub(1);
                if f.busy == 0 {
                    f.config.remove("__created");
                }
                ok(json!({ "Configuration": c, "Code": { "RepositoryType": "S3", "Location": "https://example/presigned" }, "Tags": f.tags }))
            }
            None => err(404, "ResourceNotFoundException", "Function not found"),
        },
        ("PUT", ["2015-03-31", "functions", name, "code"]) => match st.functions.get_mut(*name) {
            Some(f) if f.busy > 0 => err(409, "ResourceConflictException", "An update is in progress"),
            Some(f) => {
                let mut code = body.clone();
                if let Some(m) = code.as_object_mut()
                    && let Some(a) = m.remove("Architectures")
                {
                    f.config.insert("Architectures".into(), a);
                }
                f.code = code;
                f.sha = sha_of(&f.code);
                f.busy = 1;
                st.calls.push(format!("UpdateFunctionCode {name}"));
                ok(json!({}))
            }
            None => err(404, "ResourceNotFoundException", "Function not found"),
        },
        ("PUT", ["2015-03-31", "functions", name, "configuration"]) => match st.functions.get_mut(*name) {
            Some(f) if f.busy > 0 => err(409, "ResourceConflictException", "An update is in progress"),
            Some(f) => {
                for (k, v) in body.as_object().cloned().unwrap_or_default() {
                    f.config.insert(k, v);
                }
                f.busy = 1;
                st.calls.push(format!("UpdateFunctionConfiguration {name}"));
                ok(json!({}))
            }
            None => err(404, "ResourceNotFoundException", "Function not found"),
        },
        ("POST", ["2015-03-31", "functions", name, "versions"]) => match st.functions.get_mut(*name) {
            Some(f) => {
                let snapshot = json!({ "sha": f.sha, "config": f.config });
                if let Some((v, _, _)) = f.versions.last().filter(|(_, sha, s)| *sha == f.sha && *s == snapshot) {
                    return ok(json!({ "Version": v }));
                }
                let v = (f.versions.len() + 1).to_string();
                f.versions.push((v.clone(), f.sha.clone(), snapshot));
                st.calls.push(format!("PublishVersion {name} {v}"));
                ok(json!({ "Version": v, "CodeSha256": f.sha }))
            }
            None => err(404, "ResourceNotFoundException", "Function not found"),
        },
        ("GET", ["2015-03-31", "functions", name, "versions"]) => match st.functions.get(*name) {
            Some(f) => {
                let mut all = vec![json!({ "Version": "$LATEST" })];
                all.extend(f.versions.iter().map(|(v, sha, _)| json!({ "Version": v, "CodeSha256": sha })));
                ok(json!({ "Versions": all }))
            }
            None => err(404, "ResourceNotFoundException", "Function not found"),
        },
        ("GET", ["2015-03-31", "functions", name, "aliases", alias]) => match st.functions.get(*name).and_then(|f| f.aliases.get(*alias)) {
            Some(a) => ok(a.clone()),
            None => err(404, "ResourceNotFoundException", "Alias not found"),
        },
        ("POST", ["2015-03-31", "functions", name, "aliases"]) => match st.functions.get_mut(*name) {
            Some(f) => {
                let alias = body.get("Name").and_then(Value::as_str).unwrap_or_default().to_owned();
                let a = json!({ "Name": alias, "FunctionVersion": body.get("FunctionVersion"), "AliasArn": format!("arn:aws:lambda:us-east-1:1:function:{name}:{alias}"), "RoutingConfig": body.get("RoutingConfig") });
                f.aliases.insert(alias.clone(), a.clone());
                st.calls.push(format!("CreateAlias {name}:{alias}"));
                ok(a)
            }
            None => err(404, "ResourceNotFoundException", "Function not found"),
        },
        ("PUT", ["2015-03-31", "functions", name, "aliases", alias]) => match st.functions.get_mut(*name).and_then(|f| f.aliases.get_mut(*alias)) {
            Some(a) => {
                if let (Some(m), Some(b)) = (a.as_object_mut(), body.as_object()) {
                    for (k, v) in b {
                        m.insert(k.clone(), v.clone());
                    }
                }
                let a = a.clone();
                st.calls.push(format!("UpdateAlias {name}:{alias} {}", a.get("FunctionVersion").and_then(Value::as_str).unwrap_or_default()));
                ok(a)
            }
            None => err(404, "ResourceNotFoundException", "Alias not found"),
        },
        ("DELETE", ["2015-03-31", "functions", name, "aliases", alias]) => {
            st.functions.get_mut(*name).map(|f| f.aliases.remove(*alias));
            st.calls.push(format!("DeleteAlias {name}:{alias}"));
            srv::status(http::StatusCode::NO_CONTENT)
        }
        ("DELETE", ["2015-03-31", "functions", name]) => match st.functions.remove(*name) {
            Some(_) => {
                st.calls.push(format!("DeleteFunction {name}"));
                srv::status(http::StatusCode::NO_CONTENT)
            }
            None => err(404, "ResourceNotFoundException", "Function not found"),
        },
        ("POST", ["2015-03-31", "functions", name, "policy"]) => match st.functions.get_mut(*name) {
            Some(f) => {
                let sid = body.get("StatementId").and_then(Value::as_str).unwrap_or_default().to_owned();
                if f.policy.contains(&sid) {
                    return err(409, "ResourceConflictException", "The statement id provided already exists");
                }
                f.policy.push(sid);
                ok(json!({ "Statement": "{}" }))
            }
            None => err(404, "ResourceNotFoundException", "Function not found"),
        },
        ("POST", ["2017-03-31", "tags", arn]) => {
            let name = arn.rsplit(':').next().unwrap_or_default();
            match st.functions.get_mut(name) {
                Some(f) => {
                    for (k, v) in body.get("Tags").and_then(Value::as_object).cloned().unwrap_or_default() {
                        f.tags.insert(k, v);
                    }
                    st.calls.push(format!("TagResource {name}"));
                    srv::status(http::StatusCode::NO_CONTENT)
                }
                None => err(404, "ResourceNotFoundException", "Function not found"),
            }
        }
        (m @ ("GET" | "POST" | "PUT" | "DELETE"), ["2021-10-31", "functions", name, "url"]) => {
            let q = query.get("Qualifier").cloned().unwrap_or_default();
            let Some(f) = st.functions.get_mut(*name) else { return err(404, "ResourceNotFoundException", "Function not found") };
            match m {
                "GET" => match f.urls.get(&q) {
                    Some(u) => ok(u.clone()),
                    None => err(404, "ResourceNotFoundException", "The resource you requested does not exist."),
                },
                "POST" if f.urls.contains_key(&q) => err(409, "ResourceConflictException", "exists"),
                "POST" | "PUT" => {
                    let mut u = body.as_object().cloned().unwrap_or_default();
                    u.insert("FunctionUrl".into(), json!(format!("https://{name}.lambda-url.us-east-1.on.aws/")));
                    f.urls.insert(q, Value::Object(u.clone()));
                    ok(Value::Object(u))
                }
                _ => {
                    f.urls.remove(&q);
                    srv::status(http::StatusCode::NO_CONTENT)
                }
            }
        }
        _ => err(400, "InvalidRequestException", &format!("the fake has no {method} {path}")),
    }
}

fn s3(st: &mut State, method: &http::Method, path: &str, body: &[u8], signed: Option<&str>) -> Response {
    let mut parts = path.trim_start_matches('/').splitn(2, '/');
    let (bucket, key) = (parts.next().unwrap_or_default().to_owned(), parts.next().map(str::to_owned));
    match (method.as_str(), key) {
        ("HEAD", None) => srv::status(if st.buckets.contains_key(&bucket) { http::StatusCode::OK } else { http::StatusCode::NOT_FOUND }),
        ("PUT", None) => {
            st.buckets.entry(bucket.clone()).or_default();
            st.calls.push(format!("CreateBucket {bucket}"));
            srv::status(http::StatusCode::OK)
        }
        ("HEAD", Some(k)) => srv::status(if st.buckets.get(&bucket).is_some_and(|b| b.contains_key(&k)) { http::StatusCode::OK } else { http::StatusCode::NOT_FOUND }),
        ("PUT", Some(k)) => {
            // S3 checks the payload against the hash the request signed.
            if signed != Some(crate::util::sha256_hex(body).as_str()) {
                return srv::text(http::StatusCode::BAD_REQUEST, "<Error><Code>XAmzContentSHA256Mismatch</Code><Message>The provided 'x-amz-content-sha256' header does not match what was computed.</Message></Error>");
            }
            let Some(b) = st.buckets.get_mut(&bucket) else { return srv::text(http::StatusCode::NOT_FOUND, "<Error><Code>NoSuchBucket</Code><Message>The specified bucket does not exist</Message></Error>") };
            b.insert(k.clone(), body.to_vec());
            st.calls.push(format!("PutObject {bucket}/{k}"));
            srv::status(http::StatusCode::OK)
        }
        _ => srv::text(http::StatusCode::BAD_REQUEST, "<Error><Code>NotImplemented</Code><Message>the fake has no such S3 call</Message></Error>"),
    }
}

fn td_arn(family: &str, revision: usize) -> String {
    format!("arn:aws:ecs:us-east-1:1:task-definition/{family}:{revision}")
}

fn ecs(st: &mut State, op: &str, body: &Value) -> Response {
    let s = |k: &str| body.get(k).and_then(Value::as_str).unwrap_or_default().to_owned();
    match op {
        "RegisterTaskDefinition" => {
            let family = s("family");
            let mut td = body.as_object().cloned().unwrap_or_default();
            let tags = td.remove("tags").unwrap_or(json!([]));
            // What ECS fills in.
            if let Some(Value::Array(cs)) = td.get_mut("containerDefinitions") {
                for c in cs.iter_mut().filter_map(Value::as_object_mut) {
                    c.entry("essential").or_insert(json!(true));
                    c.entry("environment").or_insert(json!([]));
                    if let Some(Value::Array(ports)) = c.get_mut("portMappings") {
                        for p in ports.iter_mut().filter_map(Value::as_object_mut) {
                            let cp = p.get("containerPort").cloned().unwrap_or(Value::Null);
                            p.entry("hostPort").or_insert(cp);
                            p.entry("protocol").or_insert(json!("tcp"));
                        }
                    }
                }
            }
            let revs = st.task_defs.entry(family.clone()).or_default();
            let revision = revs.len() + 1;
            let arn = td_arn(&family, revision);
            td.insert("taskDefinitionArn".into(), json!(arn));
            td.insert("revision".into(), json!(revision));
            td.insert("status".into(), json!("ACTIVE"));
            td.insert("compatibilities".into(), json!(["EC2", "FARGATE"]));
            revs.push(Value::Object(td.clone()));
            st.tags.insert(arn.clone(), tags.as_array().cloned().unwrap_or_default());
            st.calls.push(format!("RegisterTaskDefinition {family}:{revision}"));
            ok(json!({ "taskDefinition": td }))
        }
        "DescribeTaskDefinition" => {
            let want = s("taskDefinition");
            let found = if want.starts_with("arn:") {
                st.task_defs.values().flatten().find(|t| t.get("taskDefinitionArn").and_then(Value::as_str) == Some(&want)).cloned()
            } else {
                st.task_defs.get(&want).and_then(|r| r.iter().rev().find(|t| t.get("status").and_then(Value::as_str) == Some("ACTIVE")).cloned())
            };
            match found {
                Some(td) => {
                    let arn = td.get("taskDefinitionArn").and_then(Value::as_str).unwrap_or_default().to_owned();
                    ok(json!({ "taskDefinition": td, "tags": st.tags.get(&arn).cloned().unwrap_or_default() }))
                }
                None => err(400, "ClientException", "Unable to describe task definition."),
            }
        }
        "TagResource" => {
            let arn = s("resourceArn");
            let mut tags = st.tags.get(&arn).cloned().unwrap_or_default();
            for t in body.get("tags").and_then(Value::as_array).cloned().unwrap_or_default() {
                tags.retain(|x| x.get("key") != t.get("key"));
                tags.push(t);
            }
            st.tags.insert(arn.clone(), tags);
            st.calls.push(format!("TagResource {arn}"));
            ok(json!({}))
        }
        "CreateService" => {
            let (cluster, name) = (s("cluster"), s("serviceName"));
            if st.services.get(&(cluster.clone(), name.clone())).is_some_and(|v| v.get("status").and_then(Value::as_str) == Some("ACTIVE")) {
                return err(400, "InvalidParameterException", "Creation of service was not idempotent.");
            }
            let arn = format!("arn:aws:ecs:us-east-1:1:service/{cluster}/{name}");
            let td = s("taskDefinition");
            let count = body.get("desiredCount").cloned().unwrap_or(json!(1));
            let mut svc = body.as_object().cloned().unwrap_or_default();
            let tags = svc.remove("tags").unwrap_or(json!([]));
            svc.remove("cluster");
            svc.insert("serviceArn".into(), json!(arn));
            svc.insert("clusterArn".into(), json!(format!("arn:aws:ecs:us-east-1:1:cluster/{cluster}")));
            svc.insert("status".into(), json!("ACTIVE"));
            svc.insert("runningCount".into(), count.clone());
            svc.insert("pendingCount".into(), json!(0));
            svc.insert("deployments".into(), json!([{ "id": "ecs-svc/1", "status": "PRIMARY", "taskDefinition": td, "desiredCount": count, "runningCount": count, "rolloutState": "COMPLETED" }]));
            st.tags.insert(arn, tags.as_array().cloned().unwrap_or_default());
            st.services.insert((cluster.clone(), name.clone()), Value::Object(svc.clone()));
            st.calls.push(format!("CreateService {cluster}/{name} {}", td.rsplit('/').next().unwrap_or_default()));
            ok(json!({ "service": svc }))
        }
        "UpdateService" => {
            let (cluster, name) = (s("cluster"), s("service"));
            let Some(Value::Object(svc)) = st.services.get_mut(&(cluster.clone(), name.clone())) else { return err(400, "ServiceNotFoundException", "Service not found.") };
            for (k, v) in body.as_object().cloned().unwrap_or_default() {
                if k != "cluster" && k != "service" {
                    svc.insert(k, v);
                }
            }
            let td = svc.get("taskDefinition").cloned().unwrap_or(Value::Null);
            let count = svc.get("desiredCount").cloned().unwrap_or(json!(1));
            svc.insert("runningCount".into(), count.clone());
            svc.insert("deployments".into(), json!([{ "id": "ecs-svc/2", "status": "PRIMARY", "taskDefinition": td, "desiredCount": count, "runningCount": count, "rolloutState": "COMPLETED" }]));
            let out = svc.clone();
            st.calls.push(format!("UpdateService {cluster}/{name} {}", td.as_str().and_then(|t| t.rsplit('/').next()).unwrap_or_default()));
            ok(json!({ "service": out }))
        }
        "DescribeServices" => {
            let cluster = s("cluster");
            let mut services = Vec::new();
            let mut failures = Vec::new();
            for n in body.get("services").and_then(Value::as_array).cloned().unwrap_or_default() {
                let n = n.as_str().unwrap_or_default().to_owned();
                match st.services.get(&(cluster.clone(), n.clone())) {
                    Some(svc) => {
                        let mut svc = svc.clone();
                        let arn = svc.get("serviceArn").and_then(Value::as_str).unwrap_or_default().to_owned();
                        if let Some(m) = svc.as_object_mut() {
                            m.insert("tags".into(), Value::Array(st.tags.get(&arn).cloned().unwrap_or_default()));
                        }
                        services.push(svc);
                    }
                    None => failures.push(json!({ "arn": n, "reason": "MISSING" })),
                }
            }
            ok(json!({ "services": services, "failures": failures }))
        }
        "DeleteService" => {
            let (cluster, name) = (s("cluster"), s("service"));
            match st.services.get_mut(&(cluster.clone(), name.clone())).and_then(Value::as_object_mut) {
                Some(svc) => {
                    svc.insert("status".into(), json!("INACTIVE"));
                    st.calls.push(format!("DeleteService {cluster}/{name}"));
                    ok(json!({}))
                }
                None => err(400, "ServiceNotFoundException", "Service not found."),
            }
        }
        "ListTaskDefinitions" => {
            let prefix = s("familyPrefix");
            let arns: Vec<Value> = st
                .task_defs
                .iter()
                .filter(|(f, _)| f.starts_with(&prefix))
                .flat_map(|(_, revs)| revs.iter().filter(|t| t.get("status").and_then(Value::as_str) == Some("ACTIVE")).filter_map(|t| t.get("taskDefinitionArn").cloned()))
                .collect();
            ok(json!({ "taskDefinitionArns": arns }))
        }
        "ListClusters" => {
            let mut names: Vec<&String> = st.services.keys().map(|(c, _)| c).collect();
            names.dedup();
            ok(json!({ "clusterArns": names.iter().map(|c| format!("arn:aws:ecs:us-east-1:1:cluster/{c}")).collect::<Vec<_>>() }))
        }
        "ListServices" => {
            let cluster = s("cluster");
            let arns: Vec<Value> = st.services.iter().filter(|((c, _), v)| *c == cluster && v.get("status").and_then(Value::as_str) == Some("ACTIVE")).filter_map(|(_, v)| v.get("serviceArn").cloned()).collect();
            ok(json!({ "serviceArns": arns }))
        }
        "DeregisterTaskDefinition" => {
            let arn = s("taskDefinition");
            for t in st.task_defs.values_mut().flatten().filter_map(Value::as_object_mut) {
                if t.get("taskDefinitionArn").and_then(Value::as_str) == Some(&arn) {
                    t.insert("status".into(), json!("INACTIVE"));
                }
            }
            st.calls.push(format!("DeregisterTaskDefinition {}", arn.rsplit('/').next().unwrap_or_default()));
            ok(json!({}))
        }
        other => err(400, "InvalidAction", &format!("the fake has no {other}")),
    }
}

/// Starts the fake on a free port: its base URL.
pub async fn start(fake: Arc<Fake>) -> anyhow::Result<String> {
    let tcp = hyper_rt::tcp::TcpListener::bind(std::net::SocketAddr::from(([127, 0, 0, 1], 0)), 64).map_err(|e| anyhow::anyhow!("{e}"))?;
    let addr = tcp.local_addr().map_err(|e| anyhow::anyhow!("{e}"))?;
    crate::rt::spawn(async move {
        while let Ok(s) = tcp.accept().await {
            let (h, r) = (Arc::clone(&fake), s.peer_addr().ok());
            crate::rt::spawn(srv::serve_connection(crate::http::io::RtIo::new(s), h, r));
        }
    });
    Ok(format!("http://{addr}"))
}

/// Records kept in memory.
#[derive(Default)]
pub struct MemRecords(pub Mutex<BTreeMap<(String, String), kiln_cd::state::Applied>>);

impl super::Records for MemRecords {
    fn get(&self, target: &str, key: &str) -> Option<kiln_cd::state::Applied> {
        self.0.lock().get(&(target.to_owned(), key.to_owned())).cloned()
    }

    fn put<'a>(&'a self, target: &'a str, key: String, applied: Option<kiln_cd::state::Applied>) -> std::pin::Pin<Box<dyn std::future::Future<Output = anyhow::Result<()>> + Send + 'a>> {
        Box::pin(async move {
            match applied {
                Some(a) => self.0.lock().insert((target.to_owned(), key), a),
                None => self.0.lock().remove(&(target.to_owned(), key)),
            };
            Ok(())
        })
    }
}

/// Artifacts' content kept in memory, by SHA-256.
#[derive(Default)]
pub struct MemFiles(pub Mutex<BTreeMap<String, Vec<u8>>>);

impl MemFiles {
    /// Keeps `data`: its SHA-256 (hex).
    pub fn add(&self, data: &[u8]) -> String {
        let hex = crate::util::sha256_hex(data);
        self.0.lock().insert(hex.clone(), data.to_vec());
        hex
    }
}

impl crate::cd::artifacts::Files for MemFiles {
    fn open<'a>(&'a self, hex: &'a str) -> std::pin::Pin<Box<dyn std::future::Future<Output = anyhow::Result<(async_channel::Receiver<bytes::Bytes>, u64)>> + Send + 'a>> {
        Box::pin(async move {
            let data = self.0.lock().get(hex).cloned().ok_or_else(|| anyhow::anyhow!("no artifact {hex}"))?;
            let (tx, rx) = async_channel::bounded(4);
            let len = data.len() as u64;
            crate::rt::spawn(async move {
                for piece in data.chunks(1000) {
                    if tx.send(bytes::Bytes::copy_from_slice(piece)).await.is_err() {
                        return;
                    }
                }
            });
            Ok((rx, len))
        })
    }
}

/// An object as the engine sends it: tracking stamped for `app`.
pub fn tracked(mut o: Value, app: &str) -> Value {
    let key = format!("{}:{}/{}:{}/{}", app, kiln_cd::obj::group(&o), kiln_cd::obj::kind(&o), kiln_cd::obj::namespace(&o), kiln_cd::obj::name(&o));
    if let Some(l) = kiln_cd::obj::ensure_map(&mut o, &["metadata", "labels"]) {
        l.insert(kiln_cd::engine::APP_LABEL.into(), json!(kiln_cd::engine::app_label(app)));
    }
    if let Some(a) = kiln_cd::obj::ensure_map(&mut o, &["metadata", "annotations"]) {
        a.insert(kiln_cd::engine::TRACKING.into(), json!(key));
    }
    o
}
