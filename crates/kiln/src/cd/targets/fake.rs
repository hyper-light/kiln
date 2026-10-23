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
    /// Its configuration's `RevisionId`, moved on by every change.
    pub revision: u64,
    /// Versions' states other than `Active` (a test makes one `Pending` or `Failed`).
    pub version_state: BTreeMap<String, String>,
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
    /// SSM: parameters' values (as SSM decrypts them) and versions, by name.
    pub parameters: BTreeMap<String, (String, i64)>,
    /// Every operation, in order: `CreateFunction thumbs`.
    pub calls: Vec<String>,
    /// An alias (`thumbs:live`) someone else moves right after kiln reads it.
    pub race: Option<String>,
}

#[derive(Default)]
pub struct Fake {
    pub state: Mutex<State>,
}

/// Tests on a fake Lambda run one at a time: the endpoints they point at theirs are the
/// process's.
pub static LAMBDA: Mutex<()> = Mutex::new(());

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
        let copy_source = srv::header_str(&req, &http::header::HeaderName::from_static("x-amz-copy-source")).map(str::to_owned);
        let method = req.method().clone();
        let path = srv::decode(req.uri().path());
        let query = srv::query(&req);
        let raw = srv::body_bytes(req.into_body(), 4 << 20).await.unwrap_or_default();
        let mut st = self.state.lock();
        // STS's Query form, S3's buckets (only S3 signs its payload's hash), ECS's JSON, Lambda's REST.
        if raw.starts_with(b"Action=GetCallerIdentity") {
            return srv::text(http::StatusCode::OK, "<GetCallerIdentityResponse><GetCallerIdentityResult><Account>123456789012</Account><Arn>arn:aws:sts::123456789012:assumed-role/kiln/x</Arn></GetCallerIdentityResult></GetCallerIdentityResponse>");
        }
        if path.starts_with("/kiln-") || signed_sha.is_some() {
            return s3(&mut st, &method, &path, &query, &raw, signed_sha.as_deref(), copy_source.as_deref());
        }
        let body: Value = serde_json::from_slice(&raw).unwrap_or(Value::Null);
        match target {
            Some(t) if t.starts_with("AmazonSSM.") => ssm(&mut st, t.rsplit('.').next().unwrap_or_default(), &body),
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
    c.insert("RevisionId".into(), json!(format!("r{}", f.revision)));
    Value::Object(c)
}

/// Lambda's refusal when an update names a `RevisionId` that is no longer the latest.
fn stale(body: &Value, current: &str) -> Option<Response> {
    let given = body.get("RevisionId").and_then(Value::as_str)?;
    (given != current).then(|| err(412, "PreconditionFailedException", "The Revision Id provided does not match the latest Revision Id. Call the GetFunction/GetAlias API to retrieve the latest Revision Id"))
}

fn without_revision(body: &Value) -> Value {
    let mut b = body.clone();
    if let Some(m) = b.as_object_mut() {
        m.remove("RevisionId");
    }
    b
}

fn alias_revision(a: &Value) -> String {
    a.get("RevisionId").and_then(Value::as_str).unwrap_or_default().to_owned()
}

fn bump_alias(a: &mut Value) {
    let n = alias_revision(a).trim_start_matches('a').parse::<u64>().unwrap_or(0) + 1;
    if let Some(m) = a.as_object_mut() {
        m.insert("RevisionId".into(), json!(format!("a{n}")));
    }
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
            let mut f = Function { busy: 1, revision: 1, ..Function::default() };
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
        // A version's (or an alias's) configuration, as published.
        ("GET", ["2015-03-31", "functions", name]) if query.get("Qualifier").is_some_and(|q| q != "$LATEST") => {
            let Some(f) = st.functions.get(*name) else { return err(404, "ResourceNotFoundException", "Function not found") };
            let q = query.get("Qualifier").cloned().unwrap_or_default();
            let v = f.aliases.get(&q).and_then(|a| a.get("FunctionVersion").and_then(Value::as_str).map(str::to_owned)).unwrap_or(q);
            match f.versions.iter().find(|(n, _, _)| *n == v) {
                Some((n, sha, snap)) => {
                    let mut c = snap.get("config").and_then(Value::as_object).cloned().unwrap_or_default();
                    c.remove("__created");
                    c.insert("FunctionName".into(), json!(name));
                    c.insert("Version".into(), json!(n));
                    c.insert("CodeSha256".into(), json!(sha));
                    c.insert("State".into(), json!(f.version_state.get(n).cloned().unwrap_or_else(|| "Active".into())));
                    c.insert("LastUpdateStatus".into(), json!("Successful"));
                    ok(json!({ "Configuration": c }))
                }
                None => err(404, "ResourceNotFoundException", "Function not found"),
            }
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
                if let Some(refused) = stale(body, &format!("r{}", f.revision)) {
                    return refused;
                }
                let mut code = without_revision(body);
                if let Some(m) = code.as_object_mut()
                    && let Some(a) = m.remove("Architectures")
                {
                    f.config.insert("Architectures".into(), a);
                }
                f.code = code;
                f.sha = sha_of(&f.code);
                f.busy = 1;
                f.revision += 1;
                st.calls.push(format!("UpdateFunctionCode {name}"));
                ok(json!({ "RevisionId": format!("r{}", f.revision) }))
            }
            None => err(404, "ResourceNotFoundException", "Function not found"),
        },
        ("PUT", ["2015-03-31", "functions", name, "configuration"]) => match st.functions.get_mut(*name) {
            Some(f) if f.busy > 0 => err(409, "ResourceConflictException", "An update is in progress"),
            Some(f) => {
                if let Some(refused) = stale(body, &format!("r{}", f.revision)) {
                    return refused;
                }
                for (k, v) in without_revision(body).as_object().cloned().unwrap_or_default() {
                    f.config.insert(k, v);
                }
                f.busy = 1;
                f.revision += 1;
                st.calls.push(format!("UpdateFunctionConfiguration {name}"));
                ok(json!({}))
            }
            None => err(404, "ResourceNotFoundException", "Function not found"),
        },
        ("POST", ["2015-03-31", "functions", name, "versions"]) => match st.functions.get_mut(*name) {
            Some(f) => {
                if let Some(refused) = stale(body, &format!("r{}", f.revision)) {
                    return refused;
                }
                if body.get("CodeSha256").and_then(Value::as_str).is_some_and(|s| s != f.sha) {
                    return err(412, "PreconditionFailedException", "CodeSHA256 does not match");
                }
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
        ("GET", ["2015-03-31", "functions", name, "aliases", alias]) => {
            let racing = st.race.as_deref() == Some(&format!("{name}:{alias}"));
            match st.functions.get_mut(*name).and_then(|f| f.aliases.get_mut(*alias)) {
                Some(a) => {
                    let read = a.clone();
                    // Someone else moves it right after this read.
                    if racing {
                        if let Some(m) = a.as_object_mut() {
                            m.insert("RoutingConfig".into(), json!({ "AdditionalVersionWeights": { "1": 0.5 } }));
                        }
                        bump_alias(a);
                    }
                    ok(read)
                }
                None => err(404, "ResourceNotFoundException", "Alias not found"),
            }
        }
        ("POST", ["2015-03-31", "functions", name, "aliases"]) => match st.functions.get_mut(*name) {
            Some(f) => {
                let alias = body.get("Name").and_then(Value::as_str).unwrap_or_default().to_owned();
                let a = json!({ "Name": alias, "FunctionVersion": body.get("FunctionVersion"), "AliasArn": format!("arn:aws:lambda:us-east-1:1:function:{name}:{alias}"), "RoutingConfig": body.get("RoutingConfig"), "RevisionId": "a1" });
                f.aliases.insert(alias.clone(), a.clone());
                st.calls.push(format!("CreateAlias {name}:{alias}"));
                ok(a)
            }
            None => err(404, "ResourceNotFoundException", "Function not found"),
        },
        ("PUT", ["2015-03-31", "functions", name, "aliases", alias]) => match st.functions.get_mut(*name).and_then(|f| f.aliases.get_mut(*alias)) {
            Some(a) => {
                if let Some(refused) = stale(body, &alias_revision(a)) {
                    return refused;
                }
                if let (Some(m), Some(b)) = (a.as_object_mut(), without_revision(body).as_object()) {
                    for (k, v) in b {
                        m.insert(k.clone(), v.clone());
                    }
                }
                bump_alias(a);
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
                    // A preview alias's URL is its own.
                    let host = if q.contains("preview") { format!("{name}-{q}") } else { (*name).to_owned() };
                    u.insert("FunctionUrl".into(), json!(format!("https://{host}.lambda-url.us-east-1.on.aws/")));
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

fn s3(st: &mut State, method: &http::Method, path: &str, query: &std::collections::HashMap<String, String>, body: &[u8], signed: Option<&str>, copy_source: Option<&str>) -> Response {
    let mut parts = path.trim_start_matches('/').splitn(2, '/');
    let (bucket, key) = (parts.next().unwrap_or_default().to_owned(), parts.next().map(str::to_owned).filter(|k| !k.is_empty()));
    let no_bucket = || srv::text(http::StatusCode::NOT_FOUND, "<Error><Code>NoSuchBucket</Code><Message>The specified bucket does not exist</Message></Error>");
    // ListObjectsV2: keys under a prefix, those past a delimiter as common prefixes.
    if method == http::Method::GET && key.is_none() && query.get("list-type").map(String::as_str) == Some("2") {
        let Some(b) = st.buckets.get(&bucket) else { return no_bucket() };
        let prefix = query.get("prefix").cloned().unwrap_or_default();
        let delimiter = query.get("delimiter").cloned().filter(|d| !d.is_empty());
        let mut contents = String::new();
        let mut common = std::collections::BTreeSet::new();
        for (k, v) in b.range(prefix.clone()..).take_while(|(k, _)| k.starts_with(&prefix)) {
            match delimiter.as_ref().and_then(|d| k.get(prefix.len()..).and_then(|rest| rest.find(d.as_str())).map(|i| (d, i))) {
                Some((d, i)) => {
                    common.insert(format!("{}{}", k.get(..prefix.len() + i).unwrap_or_default(), d));
                }
                None => contents.push_str(&format!("<Contents><Key>{k}</Key><Size>{}</Size></Contents>", v.len())),
            }
        }
        let common: String = common.iter().map(|p| format!("<CommonPrefixes><Prefix>{p}</Prefix></CommonPrefixes>")).collect();
        return srv::text(http::StatusCode::OK, &format!("<ListBucketResult><Name>{bucket}</Name><Prefix>{prefix}</Prefix><IsTruncated>false</IsTruncated>{contents}{common}</ListBucketResult>"));
    }
    match (method.as_str(), key) {
        ("GET", Some(k)) => match st.buckets.get(&bucket).and_then(|b| b.get(&k)) {
            Some(data) => srv::response(http::StatusCode::OK, "application/octet-stream", srv::full(data.clone())),
            None => srv::text(http::StatusCode::NOT_FOUND, "<Error><Code>NoSuchKey</Code><Message>The specified key does not exist.</Message></Error>"),
        },
        ("DELETE", Some(k)) => {
            if let Some(b) = st.buckets.get_mut(&bucket) {
                b.remove(&k);
            }
            st.calls.push(format!("DeleteObject {bucket}/{k}"));
            srv::status(http::StatusCode::NO_CONTENT)
        }
        // CopyObject: the source's bytes, in S3, nothing uploaded.
        ("PUT", Some(k)) if copy_source.is_some() => {
            let source = srv::decode(copy_source.unwrap_or_default());
            let mut sp = source.trim_start_matches('/').splitn(2, '/');
            let (sb, sk) = (sp.next().unwrap_or_default().to_owned(), sp.next().unwrap_or_default().to_owned());
            let Some(data) = st.buckets.get(&sb).and_then(|b| b.get(&sk)).cloned() else { return srv::text(http::StatusCode::NOT_FOUND, "<Error><Code>NoSuchKey</Code><Message>The specified key does not exist.</Message></Error>") };
            let Some(b) = st.buckets.get_mut(&bucket) else { return no_bucket() };
            b.insert(k.clone(), data);
            st.calls.push(format!("CopyObject {bucket}/{k}"));
            srv::text(http::StatusCode::OK, "<CopyObjectResult><ETag>\"fake\"</ETag></CopyObjectResult>")
        }
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

/// SSM's parameters: values are only SecureStrings here, and never answered decrypted.
fn ssm(st: &mut State, op: &str, body: &Value) -> Response {
    let name = body.get("Name").and_then(Value::as_str).unwrap_or_default().to_owned();
    match op {
        "PutParameter" => {
            if body.get("Type").and_then(Value::as_str) != Some("SecureString") {
                return err(400, "ValidationException", "the fake keeps SecureStrings");
            }
            if st.parameters.contains_key(&name) && body.get("Overwrite").and_then(Value::as_bool) != Some(true) {
                return err(400, "ParameterAlreadyExists", "The parameter already exists.");
            }
            let value = body.get("Value").and_then(Value::as_str).unwrap_or_default().to_owned();
            let version = st.parameters.get(&name).map_or(1, |(_, v)| v + 1);
            st.parameters.insert(name.clone(), (value, version));
            st.calls.push(format!("PutParameter {name}"));
            ok(json!({ "Version": version, "Tier": "Standard" }))
        }
        "GetParameters" => {
            let (mut found, mut invalid) = (Vec::new(), Vec::new());
            for n in body.get("Names").and_then(Value::as_array).into_iter().flatten().filter_map(Value::as_str) {
                match st.parameters.get(n) {
                    Some((_, v)) => found.push(json!({ "Name": n, "Type": "SecureString", "Value": "AQICAHh...encrypted", "Version": v, "ARN": format!("arn:aws:ssm:us-east-1:1:parameter{n}"), "DataType": "text" })),
                    None => invalid.push(json!(n)),
                }
            }
            ok(json!({ "Parameters": found, "InvalidParameters": invalid }))
        }
        "DeleteParameter" => match st.parameters.remove(&name) {
            Some(_) => {
                st.calls.push(format!("DeleteParameter {name}"));
                ok(json!({}))
            }
            None => err(400, "ParameterNotFound", "parameter not found"),
        },
        other => err(400, "InvalidAction", &format!("the fake has no SSM {other}")),
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

/// Records kept in memory: what kiln applied, and rollouts.
#[derive(Default)]
pub struct MemRecords(pub Mutex<BTreeMap<(String, String), kiln_cd::state::Applied>>, pub Mutex<BTreeMap<(String, String), kiln_cd::state::RolloutRecord>>);

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

    fn rollout(&self, target: &str, key: &str) -> Option<kiln_cd::state::RolloutRecord> {
        self.1.lock().get(&(target.to_owned(), key.to_owned())).cloned()
    }

    fn keep_rollout<'a>(&'a self, target: &'a str, key: &'a str, record: Option<kiln_cd::state::RolloutRecord>, expect: Option<u64>) -> std::pin::Pin<Box<dyn std::future::Future<Output = anyhow::Result<bool>> + Send + 'a>> {
        Box::pin(async move {
            let mut kept = self.1.lock();
            let k = (target.to_owned(), key.to_owned());
            let version = kept.get(&k).map(|r| r.version);
            if expect.is_some_and(|e| version != Some(e)) {
                return Ok(false);
            }
            match record {
                Some(mut r) => {
                    r.version = version.unwrap_or(0) + 1;
                    kept.insert(k, r);
                }
                None => {
                    kept.remove(&k);
                }
            }
            Ok(true)
        })
    }
}

/// Artifacts' content kept in memory, by SHA-256; and secrets' values, by their reference.
#[derive(Default)]
pub struct MemFiles(pub Mutex<BTreeMap<String, Vec<u8>>>, pub Mutex<BTreeMap<String, Vec<u8>>>);

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

    fn secret<'a>(&'a self, _app: &'a str, reference: &'a str) -> std::pin::Pin<Box<dyn std::future::Future<Output = anyhow::Result<Vec<u8>>> + Send + 'a>> {
        Box::pin(async move { self.1.lock().get(reference).cloned().ok_or_else(|| anyhow::anyhow!("no secret {reference}")) })
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

// ------------------------------------------------------------------ Cloudflare

/// A Worker on the fake Cloudflare.
#[derive(Default, Clone)]
pub struct Script {
    pub settings: Value,
    pub tags: Vec<String>,
    pub migration_tag: Option<String>,
    pub versions: u32,
    pub serving: String,
    /// Its secrets' text, by name: never answered.
    pub secrets: BTreeMap<String, String>,
}

/// The parts of Cloudflare's API the Workers provider calls, in memory.
#[derive(Default)]
pub struct CfState {
    pub scripts: BTreeMap<String, Script>,
    pub schedules: BTreeMap<String, Value>,
    pub subdomains: BTreeMap<String, bool>,
    /// Routes: (id, zone id, pattern, script).
    pub routes: Vec<(String, String, String, String)>,
    pub domains: Vec<Value>,
    /// Each upload: its metadata and its modules' names.
    pub uploads: Vec<Value>,
    next: u32,
}

impl CfState {
    /// Someone deploys a version of `name` outside kiln.
    pub fn foreign_deploy(&mut self, name: &str) {
        self.next += 1;
        let id = format!("ver-foreign-{}", self.next);
        if let Some(s) = self.scripts.get_mut(name) {
            s.serving = id;
        }
    }
}

#[derive(Default)]
pub struct FakeCf {
    pub state: Mutex<CfState>,
}

fn cf_ok(result: Value) -> Response {
    srv::json(http::StatusCode::OK, &json!({ "success": true, "errors": [], "messages": [], "result": result }))
}

fn cf_err(status: u16, code: i64, message: &str) -> Response {
    srv::json(http::StatusCode::from_u16(status).unwrap_or(http::StatusCode::BAD_REQUEST), &json!({ "success": false, "errors": [{ "code": code, "message": message }], "messages": [], "result": null }))
}

/// A multipart form's parts: (name, filename, data).
fn parts(content_type: &str, body: &[u8]) -> Vec<(String, Option<String>, Vec<u8>)> {
    let Some(boundary) = content_type.split("boundary=").nth(1) else { return Vec::new() };
    let delim = format!("--{boundary}");
    let text = body.to_vec();
    let mut out = Vec::new();
    let mut rest: &[u8] = &text;
    while let Some(i) = find(rest, delim.as_bytes()) {
        rest = rest.get(i + delim.len()..).unwrap_or_default();
        if rest.starts_with(b"--") {
            break;
        }
        let Some(h) = find(rest, b"\r\n\r\n") else { break };
        let head = String::from_utf8_lossy(rest.get(..h).unwrap_or_default()).into_owned();
        let after = rest.get(h + 4..).unwrap_or_default();
        let end = find(after, format!("\r\n{delim}").as_bytes()).unwrap_or(after.len());
        let field = |k: &str| head.split(&format!("{k}=\"")).nth(1).and_then(|r| r.split('"').next()).map(str::to_owned);
        out.push((field("name").unwrap_or_default(), field("filename"), after.get(..end).unwrap_or_default().to_vec()));
        rest = after.get(end..).unwrap_or_default();
    }
    out
}

fn find(hay: &[u8], needle: &[u8]) -> Option<usize> {
    hay.windows(needle.len()).position(|w| w == needle)
}

impl srv::Handler for FakeCf {
    async fn handle(self: Arc<Self>, req: Request) -> Response {
        if srv::header_str(&req, &http::header::AUTHORIZATION) != Some("Bearer cf-token") {
            return cf_err(403, 10000, "Authentication error");
        }
        let method = req.method().clone();
        let path = req.uri().path().to_owned();
        let query = srv::query(&req);
        let ct = srv::header_str(&req, &http::header::CONTENT_TYPE).unwrap_or("").to_owned();
        let raw = srv::body_bytes(req.into_body(), 8 << 20).await.unwrap_or_default();
        let body: Value = serde_json::from_slice(&raw).unwrap_or(Value::Null);
        let mut st = self.state.lock();
        let segs: Vec<String> = path.trim_start_matches('/').split('/').map(srv::decode).collect();
        let segs: Vec<&str> = segs.iter().map(String::as_str).collect();
        match (method.as_str(), segs.as_slice()) {
            ("GET", ["accounts", "acct1", "workers", "scripts"]) => {
                cf_ok(Value::Array(st.scripts.iter().map(|(n, s)| json!({ "id": n, "tags": s.tags, "migration_tag": s.migration_tag, "modified_on": "2026-10-18T00:00:00Z", "etag": format!("e{}", s.versions) })).collect()))
            }
            ("PUT", ["accounts", "acct1", "workers", "scripts", name]) => {
                let ps = parts(&ct, &raw);
                let Some(meta) = ps.iter().find(|p| p.0 == "metadata").and_then(|p| serde_json::from_slice::<Value>(&p.2).ok()) else { return cf_err(400, 10021, "no metadata") };
                let main = meta.get("main_module").and_then(Value::as_str).unwrap_or_default();
                if !ps.iter().any(|p| p.0 == main) {
                    return cf_err(400, 10021, "main_module isn't among the modules");
                }
                let mut s = st.scripts.get(*name).cloned().unwrap_or_default();
                if let Some(m) = meta.get("migrations") {
                    let old = m.get("old_tag").and_then(Value::as_str);
                    if old != s.migration_tag.as_deref() {
                        return cf_err(400, 10074, "migration old_tag doesn't match the Worker's");
                    }
                    s.migration_tag = m.get("new_tag").and_then(Value::as_str).map(str::to_owned);
                }
                let mut settings = meta.as_object().cloned().unwrap_or_default();
                for k in ["main_module", "migrations", "tags", "keep_bindings"] {
                    settings.remove(k);
                }
                // Secrets are kept apart: those the upload names, and (kept) the ones it had.
                if !meta.get("keep_bindings").and_then(Value::as_array).is_some_and(|k| k.iter().any(|t| t == "secret_text")) {
                    s.secrets.clear();
                }
                let mut plain = Vec::new();
                for b in settings.get("bindings").and_then(Value::as_array).cloned().unwrap_or_default() {
                    match (b.get("type").and_then(Value::as_str), b.get("name").and_then(Value::as_str)) {
                        (Some("secret_text"), Some(n)) => {
                            s.secrets.insert(n.to_owned(), b.get("text").and_then(Value::as_str).unwrap_or_default().to_owned());
                        }
                        _ => plain.push(b),
                    }
                }
                plain.extend(s.secrets.keys().map(|n| json!({ "type": "secret_text", "name": n })));
                settings.insert("bindings".into(), Value::Array(plain));
                s.settings = Value::Object(settings);
                s.tags = meta.get("tags").and_then(Value::as_array).into_iter().flatten().filter_map(Value::as_str).map(str::to_owned).collect();
                s.versions += 1;
                st.next += 1;
                s.serving = format!("ver-{}", st.next);
                st.scripts.insert((*name).to_owned(), s);
                let modules: Vec<String> = ps.iter().filter(|p| p.1.is_some()).map(|p| p.0.clone()).collect();
                st.uploads.push(json!({ "metadata": meta, "modules": modules }));
                cf_ok(json!({ "id": name }))
            }
            ("GET", ["accounts", "acct1", "workers", "scripts", name, what]) => match (st.scripts.get(*name), *what) {
                (None, _) => cf_err(404, 10007, "workers.api.error.script_not_found"),
                (Some(s), "settings") => {
                    let mut v = s.settings.clone();
                    if let Value::Object(m) = &mut v {
                        m.insert("tags".into(), json!(s.tags));
                    }
                    cf_ok(v)
                }
                (Some(s), "deployments") => cf_ok(json!({ "deployments": [{ "id": format!("dep-{}", s.serving), "versions": [{ "version_id": s.serving, "percentage": 100.0 }] }] })),
                (Some(_), "schedules") => cf_ok(json!({ "schedules": st.schedules.get(*name).cloned().unwrap_or(json!([])).as_array().into_iter().flatten().map(|c| json!({ "cron": c })).collect::<Vec<_>>() })),
                (Some(_), "subdomain") => cf_ok(json!({ "enabled": st.subdomains.get(*name).copied().unwrap_or(false) })),
                _ => cf_err(404, 7003, "no route"),
            },
            ("PUT", ["accounts", "acct1", "workers", "scripts", name, "schedules"]) => {
                st.schedules.insert((*name).to_owned(), Value::Array(body.as_array().into_iter().flatten().filter_map(|c| c.get("cron").cloned()).collect()));
                cf_ok(json!({}))
            }
            ("POST", ["accounts", "acct1", "workers", "scripts", name, "subdomain"]) => {
                st.subdomains.insert((*name).to_owned(), body.get("enabled").and_then(Value::as_bool).unwrap_or(false));
                cf_ok(json!({}))
            }
            // A secret deleted: a new version without it, deployed.
            ("DELETE", ["accounts", "acct1", "workers", "scripts", name, "secrets", secret]) => {
                st.next += 1;
                let serving = format!("ver-{}", st.next);
                let Some(s) = st.scripts.get_mut(*name) else { return cf_err(404, 10007, "workers.api.error.script_not_found") };
                if s.secrets.remove(*secret).is_none() {
                    return cf_err(404, 10056, "workers.api.error.secret_not_found");
                }
                if let Some(Value::Array(b)) = s.settings.get_mut("bindings") {
                    b.retain(|x| x.get("name").and_then(Value::as_str) != Some(*secret));
                }
                s.versions += 1;
                s.serving = serving;
                cf_ok(Value::Null)
            }
            ("DELETE", ["accounts", "acct1", "workers", "scripts", name]) => match st.scripts.remove(*name) {
                Some(_) => cf_ok(Value::Null),
                None => cf_err(404, 10007, "workers.api.error.script_not_found"),
            },
            ("GET", ["zones"]) => cf_ok(if query.get("name").map(String::as_str) == Some("example.com") { json!([{ "id": "zone1", "name": "example.com" }]) } else { json!([]) }),
            ("GET", ["zones", zone, "workers", "routes"]) => cf_ok(Value::Array(st.routes.iter().filter(|r| r.1 == *zone).map(|r| json!({ "id": r.0, "pattern": r.2, "script": r.3 })).collect())),
            ("POST", ["zones", zone, "workers", "routes"]) => {
                st.next += 1;
                let id = format!("route-{}", st.next);
                let (pattern, script) = (body.get("pattern").and_then(Value::as_str).unwrap_or_default().to_owned(), body.get("script").and_then(Value::as_str).unwrap_or_default().to_owned());
                st.routes.push((id.clone(), (*zone).to_owned(), pattern, script));
                cf_ok(json!({ "id": id }))
            }
            ("PUT", ["zones", _zone, "workers", "routes", id]) => {
                if let Some(r) = st.routes.iter_mut().find(|r| r.0 == *id) {
                    r.3 = body.get("script").and_then(Value::as_str).unwrap_or_default().to_owned();
                }
                cf_ok(json!({ "id": id }))
            }
            ("DELETE", ["zones", _zone, "workers", "routes", id]) => {
                st.routes.retain(|r| r.0 != *id);
                cf_ok(Value::Null)
            }
            ("GET", ["accounts", "acct1", "workers", "domains"]) => {
                let host = query.get("hostname").cloned().unwrap_or_default();
                cf_ok(Value::Array(st.domains.iter().filter(|d| host.is_empty() || d.get("hostname").and_then(Value::as_str) == Some(&host)).cloned().collect()))
            }
            ("PUT", ["accounts", "acct1", "workers", "domains"]) => {
                let host = body.get("hostname").and_then(Value::as_str).unwrap_or_default().to_owned();
                st.domains.retain(|d| d.get("hostname").and_then(Value::as_str) != Some(&host));
                st.next += 1;
                let d = json!({ "id": format!("dom-{}", st.next), "hostname": host, "service": body.get("service"), "environment": "production", "zone_id": body.get("zone_id"), "zone_name": "example.com" });
                st.domains.push(d.clone());
                cf_ok(d)
            }
            ("DELETE", ["accounts", "acct1", "workers", "domains", id]) => {
                st.domains.retain(|d| d.get("id").and_then(Value::as_str) != Some(*id));
                cf_ok(Value::Null)
            }
            _ => cf_err(400, 7000, &format!("the fake has no {method} {path}")),
        }
    }
}

/// Starts the fake Cloudflare on a free port: its base URL.
pub async fn start_cf(fake: Arc<FakeCf>) -> anyhow::Result<String> {
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

// ------------------------------------------------------------------ Vercel

/// The parts of Vercel's API the provider calls, in memory.
#[derive(Default)]
pub struct VercelState {
    pub projects: BTreeMap<String, Value>,
    /// Each deployment made: its body, id and state.
    pub deployments: Vec<Value>,
    /// Files Vercel has, by SHA-1.
    pub known: std::collections::BTreeSet<String>,
    pub uploads: u32,
    pub domains: BTreeMap<(String, String), Value>,
    /// Projects' environment variables, with their values (a sensitive one's never answered).
    pub env: BTreeMap<String, Vec<Value>>,
    next: u32,
}

impl VercelState {
    /// A deployment made outside kiln serves `project`'s production now.
    pub fn foreign_production(&mut self, project: &str) {
        self.next += 1;
        let id = format!("dpl_foreign{}", self.next);
        self.deployments.push(json!({ "id": id, "readyState": "READY", "polls": 0 }));
        if let Some(Value::Object(p)) = self.projects.get_mut(project) {
            p.insert("targets".into(), json!({ "production": { "id": id } }));
        }
    }
}

#[derive(Default)]
pub struct FakeVercel {
    pub state: Mutex<VercelState>,
}

fn vc_err(status: u16, code: &str, message: &str, extra: Value) -> Response {
    let mut e = json!({ "code": code, "message": message });
    if let (Value::Object(m), Value::Object(x)) = (&mut e, extra) {
        m.extend(x);
    }
    srv::json(http::StatusCode::from_u16(status).unwrap_or(http::StatusCode::BAD_REQUEST), &json!({ "error": e }))
}

impl srv::Handler for FakeVercel {
    async fn handle(self: Arc<Self>, req: Request) -> Response {
        if srv::header_str(&req, &http::header::AUTHORIZATION) != Some("Bearer vc-token") {
            return vc_err(403, "forbidden", "Not authorized", json!({}));
        }
        let query = srv::query(&req);
        if query.get("teamId").map(String::as_str) != Some("team1") {
            return vc_err(403, "forbidden", "Not authorized for this team", json!({}));
        }
        let digest = srv::header_str(&req, &http::header::HeaderName::from_static("x-vercel-digest")).map(str::to_owned);
        let method = req.method().clone();
        let path = req.uri().path().to_owned();
        let raw = srv::body_bytes(req.into_body(), 8 << 20).await.unwrap_or_default();
        let body: Value = serde_json::from_slice(&raw).unwrap_or(Value::Null);
        let mut st = self.state.lock();
        let segs: Vec<String> = path.trim_start_matches('/').split('/').map(srv::decode).collect();
        let segs: Vec<&str> = segs.iter().map(String::as_str).collect();
        match (method.as_str(), segs.as_slice()) {
            ("POST", ["v2", "files"]) => {
                let sha = crate::ci::integrations::vercel::sha1_hex(&raw);
                if digest.as_deref() != Some(sha.as_str()) {
                    return vc_err(400, "invalid_digest", "the file doesn't hash to its digest", json!({}));
                }
                st.known.insert(sha);
                st.uploads += 1;
                ok(json!({}))
            }
            ("GET", ["v9", "projects"]) => ok(json!({ "projects": st.projects.values().cloned().collect::<Vec<_>>() })),
            ("POST", ["v11", "projects"]) => {
                st.next += 1;
                let mut p = body.as_object().cloned().unwrap_or_default();
                p.insert("id".into(), json!(format!("prj_{}", st.next)));
                let name = p.get("name").and_then(Value::as_str).unwrap_or_default().to_owned();
                st.projects.insert(name, Value::Object(p.clone()));
                ok(Value::Object(p))
            }
            ("GET", ["v9", "projects", name]) => match st.projects.get(*name) {
                Some(p) => ok(p.clone()),
                None => vc_err(404, "not_found", "Project not found", json!({})),
            },
            ("PATCH", ["v9", "projects", name]) => match st.projects.get_mut(*name) {
                Some(Value::Object(p)) => {
                    for (k, v) in body.as_object().cloned().unwrap_or_default() {
                        p.insert(k, v);
                    }
                    ok(Value::Object(p.clone()))
                }
                _ => vc_err(404, "not_found", "Project not found", json!({})),
            },
            ("DELETE", ["v9", "projects", name]) => match st.projects.remove(*name) {
                Some(_) => srv::status(http::StatusCode::NO_CONTENT),
                None => vc_err(404, "not_found", "Project not found", json!({})),
            },
            ("POST", ["v13", "deployments"]) => {
                let project = body.get("project").and_then(Value::as_str).unwrap_or_default().to_owned();
                if !st.projects.contains_key(&project) {
                    return vc_err(404, "not_found", "Project not found", json!({}));
                }
                let lacks: Vec<String> = body.get("files").and_then(Value::as_array).into_iter().flatten().filter_map(|f| f.get("sha").and_then(Value::as_str)).filter(|s| !st.known.contains(*s)).map(str::to_owned).collect();
                if !lacks.is_empty() {
                    return vc_err(400, "missing_files", "Missing files", json!({ "missing": lacks }));
                }
                st.next += 1;
                let id = format!("dpl_{}", st.next);
                let mut d = body.as_object().cloned().unwrap_or_default();
                // A production deployment takes the project's production variables as they are now.
                let vars: Map<String, Value> = st.env.get(&project).into_iter().flatten().filter(|e| e.get("target").and_then(Value::as_array).is_some_and(|t| t.contains(&json!("production")))).filter_map(|e| Some((e.get("key")?.as_str()?.to_owned(), e.get("value")?.clone()))).collect();
                d.insert("env".into(), Value::Object(vars));
                d.insert("id".into(), json!(id));
                d.insert("url".into(), json!(format!("site-{}.vercel.app", st.next)));
                d.insert("readyState".into(), json!("BUILDING"));
                d.insert("polls".into(), json!(0));
                st.deployments.push(Value::Object(d.clone()));
                ok(Value::Object(d))
            }
            ("GET", ["v13", "deployments", id]) => {
                let Some(d) = st.deployments.iter_mut().find(|d| d.get("id").and_then(Value::as_str) == Some(*id)).and_then(Value::as_object_mut) else { return vc_err(404, "not_found", "Deployment not found", json!({})) };
                // Ready after one look; production then serves it.
                let polls = d.get("polls").and_then(Value::as_u64).unwrap_or(0) + 1;
                d.insert("polls".into(), json!(polls));
                if polls > 1 && d.get("readyState") == Some(&json!("BUILDING")) {
                    d.insert("readyState".into(), json!("READY"));
                }
                let d = Value::Object(d.clone());
                if d.get("readyState") == Some(&json!("READY"))
                    && d.get("target") == Some(&json!("production"))
                    && let Some(Value::Object(p)) = d.get("project").and_then(Value::as_str).and_then(|p| st.projects.get_mut(p))
                    && p.get("targets").and_then(|t| t.pointer("/production/id")).is_none_or(|cur| cur.as_str().is_some_and(|c| !c.starts_with("dpl_foreign")))
                {
                    p.insert("targets".into(), json!({ "production": { "id": id } }));
                }
                ok(d)
            }
            ("GET", ["v9", "projects", project, "env"]) => {
                let envs: Vec<Value> = st
                    .env
                    .get(*project)
                    .into_iter()
                    .flatten()
                    .map(|e| {
                        let mut e = e.clone();
                        if let Value::Object(m) = &mut e
                            && m.get("type") == Some(&json!("sensitive"))
                        {
                            m.remove("value");
                        }
                        e
                    })
                    .collect();
                ok(json!({ "envs": envs }))
            }
            ("POST", ["v10", "projects", project, "env"]) => {
                if !st.projects.contains_key(*project) {
                    return vc_err(404, "not_found", "Project not found", json!({}));
                }
                let items: Vec<Value> = match &body {
                    Value::Array(a) => a.clone(),
                    one => vec![one.clone()],
                };
                let mut created = Vec::new();
                for item in items {
                    st.next += 1;
                    let n = st.next;
                    let key = item.get("key").cloned().unwrap_or(Value::Null);
                    let list = st.env.entry((*project).to_owned()).or_default();
                    if query.get("upsert").map(String::as_str) != Some("true") && list.iter().any(|e| e.get("key") == Some(&key)) {
                        return vc_err(400, "ENV_ALREADY_EXISTS", "A variable with the name already exists", json!({}));
                    }
                    list.retain(|e| e.get("key") != Some(&key));
                    let mut e = item.as_object().cloned().unwrap_or_default();
                    e.insert("id".into(), json!(format!("env_{n}")));
                    e.insert("updatedAt".into(), json!(1_760_000_000_000_u64 + u64::from(n)));
                    list.push(Value::Object(e.clone()));
                    e.remove("value");
                    created.push(Value::Object(e));
                }
                ok(json!({ "created": created }))
            }
            ("DELETE", ["v9", "projects", project, "env", id]) => {
                if let Some(list) = st.env.get_mut(*project) {
                    list.retain(|e| e.get("id").and_then(Value::as_str) != Some(*id));
                }
                ok(json!({}))
            }
            ("GET", ["v9", "projects", project, "domains"]) => ok(json!({ "domains": st.domains.iter().filter(|((p, _), _)| p == project).map(|(_, d)| d.clone()).collect::<Vec<_>>() })),
            ("GET", ["v9", "projects", project, "domains", name]) => match st.domains.get(&((*project).to_owned(), (*name).to_owned())) {
                Some(d) => ok(d.clone()),
                None => vc_err(404, "not_found", "Domain not found", json!({})),
            },
            ("POST", ["v10", "projects", project, "domains"]) => {
                let name = body.get("name").and_then(Value::as_str).unwrap_or_default().to_owned();
                let d = json!({ "name": name, "verified": true, "projectId": project });
                st.domains.insert(((*project).to_owned(), name), d.clone());
                ok(d)
            }
            ("DELETE", ["v9", "projects", project, "domains", name]) => {
                st.domains.remove(&((*project).to_owned(), (*name).to_owned()));
                srv::status(http::StatusCode::NO_CONTENT)
            }
            _ => vc_err(400, "bad_request", &format!("the fake has no {method} {path}"), json!({})),
        }
    }
}

/// Starts the fake Vercel on a free port: its base URL.
pub async fn start_vercel(fake: Arc<FakeVercel>) -> anyhow::Result<String> {
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
