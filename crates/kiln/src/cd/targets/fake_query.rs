//! A fake of AWS's Query-protocol services for tests (CloudFormation, Elastic Beanstalk, EC2 and
//! Auto Scaling), in memory, behaving as AWS documents them where kiln depends on it: a change set
//! is worked out before it can run and one that changes nothing fails saying so; a stack is busy
//! while it changes and an update that fails rolls back. Requests must be signed. And a release
//! as KilnCD renders it for a provider ([`released`]): the tests' way through a release.

use std::collections::BTreeMap;
use std::sync::Arc;

use parking_lot::Mutex;
use serde_json::{Map, Value, json};

use crate::http::server::{self as srv, Request, Response};

/// A stack as the fake keeps it.
#[derive(Clone, Debug, Default)]
pub struct Stack {
    pub id: String,
    pub status: String,
    pub reason: String,
    pub template: String,
    pub parameters: BTreeMap<String, String>,
    pub capabilities: Vec<String>,
    pub tags: BTreeMap<String, String>,
    /// Describes left before what it's doing is done.
    pub busy: u32,
    /// What it becomes once done.
    pub then: String,
}

/// A change set as the fake keeps it.
#[derive(Clone, Debug, Default)]
pub struct ChangeSet {
    pub stack: String,
    pub kind: String,
    pub template: String,
    pub parameters: BTreeMap<String, String>,
    pub capabilities: Vec<String>,
    pub tags: BTreeMap<String, String>,
    pub status: String,
    pub reason: String,
    /// Describes left before it's worked out.
    pub busy: u32,
}

#[derive(Default)]
pub struct State {
    pub stacks: BTreeMap<String, Stack>,
    pub change_sets: BTreeMap<String, ChangeSet>,
    /// Every operation, in order: `ExecuteChangeSet shop-network`.
    pub calls: Vec<String>,
}

#[derive(Default)]
pub struct FakeQuery {
    pub state: Mutex<State>,
}

/// A Query-protocol answer: `<{action}Response><{action}Result>…</…>`.
fn answer(action: &str, result: &Value) -> Response {
    let body = format!("<{action}Response xmlns=\"https://fake.aws/\"><{action}Result>{}</{action}Result><ResponseMetadata><RequestId>fake</RequestId></ResponseMetadata></{action}Response>", to_xml(result, "member"));
    srv::text(http::StatusCode::OK, &body)
}

/// A Query-protocol error.
fn error(status: u16, code: &str, message: &str) -> Response {
    let body = format!("<ErrorResponse><Error><Type>Sender</Type><Code>{code}</Code><Message>{}</Message></Error><RequestId>fake</RequestId></ErrorResponse>", escape(message));
    srv::text(http::StatusCode::from_u16(status).unwrap_or(http::StatusCode::BAD_REQUEST), &body)
}

fn escape(s: &str) -> String {
    s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;")
}

/// JSON as AWS's XML has it: an object's fields elements, an array's items `<{item}>`.
pub fn to_xml(v: &Value, item: &str) -> String {
    match v {
        Value::Object(m) => m.iter().map(|(k, x)| format!("<{k}>{}</{k}>", to_xml(x, item))).collect(),
        Value::Array(a) => a.iter().map(|x| format!("<{item}>{}</{item}>", to_xml(x, item))).collect(),
        Value::String(s) => escape(s),
        Value::Null => String::new(),
        other => other.to_string(),
    }
}

/// A Query form's fields.
fn form(raw: &[u8]) -> BTreeMap<String, String> {
    String::from_utf8_lossy(raw)
        .split('&')
        .filter(|kv| !kv.is_empty())
        .map(|kv| {
            let (k, v) = kv.split_once('=').unwrap_or((kv, ""));
            (srv::decode(&k.replace('+', "%2B")), srv::decode(&v.replace('+', "%2B")))
        })
        .collect()
}

/// A form's list: `Name.member.1`, `Name.member.2`, … in order.
fn list(f: &BTreeMap<String, String>, name: &str) -> Vec<String> {
    (1..).map_while(|i| f.get(&format!("{name}.member.{i}")).cloned()).collect()
}

/// A form's pairs: `Name.member.N.{key}` and `.{value}`.
fn pairs(f: &BTreeMap<String, String>, name: &str, key: &str, value: &str) -> BTreeMap<String, String> {
    (1..).map_while(|i| Some((f.get(&format!("{name}.member.{i}.{key}"))?.clone(), f.get(&format!("{name}.member.{i}.{value}")).cloned().unwrap_or_default()))).collect()
}

impl srv::Handler for FakeQuery {
    async fn handle(self: Arc<Self>, req: Request) -> Response {
        let auth = srv::header_str(&req, &http::header::AUTHORIZATION).unwrap_or("").to_owned();
        if !auth.starts_with("AWS4-HMAC-SHA256 Credential=") {
            return error(403, "InvalidClientTokenId", "unsigned");
        }
        let raw = srv::body_bytes(req.into_body(), 4 << 20).await.unwrap_or_default();
        let f = form(&raw);
        let action = f.get("Action").cloned().unwrap_or_default();
        let mut st = self.state.lock();
        match f.get("Version").map(String::as_str) {
            Some("2010-05-15") => cloudformation(&mut st, &action, &f),
            _ => error(400, "InvalidAction", &format!("the fake takes no {action}")),
        }
    }
}

/// The resources a template names, as the fake makes them: the keys under `Resources:` (YAML) or
/// `"Resources"` (JSON), with their types.
fn resources_of(template: &str) -> Vec<(String, String)> {
    if let Ok(v) = serde_json::from_str::<Value>(template) {
        return v.get("Resources").and_then(Value::as_object).into_iter().flatten().map(|(k, r)| (k.clone(), r.get("Type").and_then(Value::as_str).unwrap_or_default().to_owned())).collect();
    }
    let mut out = Vec::new();
    let mut inside = false;
    let mut current: Option<String> = None;
    for line in template.lines() {
        if !line.starts_with(' ') && !line.trim().is_empty() {
            inside = line.trim_end() == "Resources:";
            continue;
        }
        if !inside {
            continue;
        }
        let indent = line.len() - line.trim_start().len();
        let t = line.trim();
        if indent == 2 && t.ends_with(':') {
            current = Some(t.trim_end_matches(':').to_owned());
            out.push((t.trim_end_matches(':').to_owned(), String::new()));
        } else if let (Some(name), Some(ty)) = (&current, t.strip_prefix("Type:")) {
            if let Some(last) = out.iter_mut().rev().find(|(n, _)| n == name) {
                last.1 = ty.trim().to_owned();
            }
        }
    }
    out
}

fn stack_json(name: &str, s: &Stack) -> Value {
    let params: Vec<Value> = s.parameters.iter().map(|(k, v)| json!({ "ParameterKey": k, "ParameterValue": if k.starts_with("Secret") { "****" } else { v.as_str() } })).collect();
    let tags: Vec<Value> = s.tags.iter().map(|(k, v)| json!({ "Key": k, "Value": v })).collect();
    json!({ "StackName": name, "StackId": s.id, "StackStatus": s.status, "StackStatusReason": s.reason, "Parameters": params, "Capabilities": s.capabilities, "Tags": tags, "Outputs": [{ "OutputKey": "Env", "OutputValue": s.parameters.get("Env").cloned().unwrap_or_default() }] })
}

fn cloudformation(st: &mut State, action: &str, f: &BTreeMap<String, String>) -> Response {
    let stack_name = f.get("StackName").cloned().unwrap_or_default();
    let missing = || error(400, "ValidationError", &format!("Stack with id {stack_name} does not exist"));
    // Whatever was under way when a stack is looked at moves on a step.
    if let Some(s) = st.stacks.get_mut(&stack_name)
        && action == "DescribeStacks"
        && s.busy > 0
    {
        s.busy -= 1;
        if s.busy == 0 {
            s.status = std::mem::take(&mut s.then);
            if s.status == "DELETE_COMPLETE" {
                st.stacks.remove(&stack_name);
            }
        }
    }
    match action {
        "DescribeStacks" => {
            if stack_name.is_empty() {
                let all: Vec<Value> = st.stacks.iter().map(|(n, s)| stack_json(n, s)).collect();
                return answer(action, &json!({ "Stacks": all }));
            }
            match st.stacks.get(&stack_name) {
                Some(s) => answer(action, &json!({ "Stacks": [stack_json(&stack_name, s)] })),
                None => missing(),
            }
        }
        "GetTemplate" => match st.stacks.get(&stack_name) {
            Some(s) => answer(action, &json!({ "TemplateBody": s.template, "StagesAvailable": ["Original", "Processed"] })),
            None => missing(),
        },
        "DescribeStackResources" => match st.stacks.get(&stack_name) {
            Some(s) => {
                let rs: Vec<Value> = resources_of(&s.template).into_iter().map(|(n, t)| json!({ "LogicalResourceId": n, "ResourceType": t, "PhysicalResourceId": format!("{stack_name}-{n}"), "ResourceStatus": if s.status.contains("ROLLBACK") { "UPDATE_COMPLETE" } else { "CREATE_COMPLETE" } })).collect();
                answer(action, &json!({ "StackResources": rs }))
            }
            None => missing(),
        },
        "CreateChangeSet" => {
            let name = f.get("ChangeSetName").cloned().unwrap_or_default();
            let kind = f.get("ChangeSetType").cloned().unwrap_or_else(|| "UPDATE".into());
            let template = f.get("TemplateBody").cloned().or_else(|| f.get("TemplateURL").map(|u| format!("from {u}"))).unwrap_or_default();
            if st.change_sets.contains_key(&format!("{stack_name}/{name}")) {
                return error(400, "AlreadyExistsException", &format!("ChangeSet {name} already exists"));
            }
            let exists = st.stacks.get(&stack_name).is_some_and(|s| s.status != "REVIEW_IN_PROGRESS");
            match (kind.as_str(), exists) {
                ("CREATE", true) => return error(400, "AlreadyExistsException", &format!("Stack [{stack_name}] already exists")),
                ("UPDATE", false) => return missing(),
                _ => {}
            }
            let cs = ChangeSet {
                stack: stack_name.clone(),
                kind: kind.clone(),
                template,
                parameters: pairs(f, "Parameters", "ParameterKey", "ParameterValue"),
                capabilities: list(f, "Capabilities"),
                tags: pairs(f, "Tags", "Key", "Value"),
                status: "CREATE_PENDING".into(),
                reason: String::new(),
                busy: 1,
            };
            if kind == "CREATE" {
                st.stacks.insert(stack_name.clone(), Stack { id: format!("arn:aws:cloudformation:us-east-1:123456789012:stack/{stack_name}/1"), status: "REVIEW_IN_PROGRESS".into(), ..Stack::default() });
            }
            st.calls.push(format!("CreateChangeSet {stack_name} {kind}"));
            let id = format!("arn:aws:cloudformation:us-east-1:123456789012:changeSet/{name}/1");
            st.change_sets.insert(format!("{stack_name}/{name}"), cs);
            answer(action, &json!({ "Id": id, "StackId": st.stacks.get(&stack_name).map(|s| s.id.clone()) }))
        }
        "DescribeChangeSet" => {
            let key = format!("{stack_name}/{}", f.get("ChangeSetName").cloned().unwrap_or_default());
            let stack = st.stacks.get(&stack_name).cloned();
            let Some(cs) = st.change_sets.get_mut(&key) else { return error(404, "ChangeSetNotFound", &format!("ChangeSet {key} does not exist")) };
            if cs.busy > 0 {
                cs.busy -= 1;
            } else if cs.status == "CREATE_PENDING" {
                // Worked out: one that changes nothing fails, saying so.
                let same = stack.is_some_and(|s| s.template == cs.template && s.parameters == cs.parameters && s.capabilities == cs.capabilities && s.tags == cs.tags);
                if same {
                    cs.status = "FAILED".into();
                    cs.reason = "The submitted information didn't contain changes. Submit different information to create a change set.".into();
                } else if cs.template.contains("Transform: AWS::Serverless") && !cs.capabilities.iter().any(|c| c == "CAPABILITY_AUTO_EXPAND") {
                    cs.status = "FAILED".into();
                    cs.reason = "Requires capabilities : [CAPABILITY_AUTO_EXPAND]".into();
                } else {
                    cs.status = "CREATE_COMPLETE".into();
                }
            }
            let execution = if cs.status == "CREATE_COMPLETE" { "AVAILABLE" } else { "UNAVAILABLE" };
            answer(action, &json!({ "Status": cs.status, "StatusReason": cs.reason, "ExecutionStatus": execution, "ChangeSetType": cs.kind }))
        }
        "ExecuteChangeSet" => {
            let key = format!("{stack_name}/{}", f.get("ChangeSetName").cloned().unwrap_or_default());
            let Some(cs) = st.change_sets.remove(&key) else { return error(404, "ChangeSetNotFound", &format!("ChangeSet {key} does not exist")) };
            if cs.status != "CREATE_COMPLETE" {
                st.change_sets.insert(key, cs);
                return error(400, "InvalidChangeSetStatus", "the change set isn't AVAILABLE");
            }
            let Some(s) = st.stacks.get_mut(&stack_name) else { return missing() };
            let creating = cs.kind == "CREATE";
            // A template saying FAIL_ON_UPDATE fails as it updates, and CloudFormation rolls it back.
            if !creating && cs.template.contains("FAIL_ON_UPDATE") {
                s.status = "UPDATE_IN_PROGRESS".into();
                s.then = "UPDATE_ROLLBACK_COMPLETE".into();
                s.reason = "Resource handler returned message: \"Bucket shop-broken already exists\"".into();
                s.busy = 2;
            } else {
                s.template = cs.template;
                s.parameters = cs.parameters;
                s.capabilities = cs.capabilities;
                s.tags = cs.tags;
                s.status = if creating { "CREATE_IN_PROGRESS".into() } else { "UPDATE_IN_PROGRESS".into() };
                s.then = if creating { "CREATE_COMPLETE".into() } else { "UPDATE_COMPLETE".into() };
                s.reason = String::new();
                s.busy = 2;
            }
            st.calls.push(format!("ExecuteChangeSet {stack_name}"));
            answer(action, &json!({}))
        }
        "DeleteChangeSet" => {
            st.change_sets.remove(&format!("{stack_name}/{}", f.get("ChangeSetName").cloned().unwrap_or_default()));
            st.calls.push(format!("DeleteChangeSet {stack_name}"));
            answer(action, &json!({}))
        }
        "DeleteStack" => {
            if let Some(s) = st.stacks.get_mut(&stack_name) {
                s.status = "DELETE_IN_PROGRESS".into();
                s.then = "DELETE_COMPLETE".into();
                s.busy = 1;
                st.calls.push(format!("DeleteStack {stack_name}"));
            }
            answer(action, &json!({}))
        }
        other => error(400, "InvalidAction", &format!("CloudFormation has no {other} in the fake")),
    }
}

pub async fn start(fake: Arc<FakeQuery>) -> anyhow::Result<String> {
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

// ------------------------------------------------------------------ through a release

/// What a release of `provider` with `spec` (`.kiln/deploy.yaml`'s, its `${{ release.* }}` filled
/// by the release) at `version` makes KilnCD apply: the definition kiln's deploy makes of the
/// release (`crate::deploy::kilncd::definition`), rendered for its target as KilnCD renders it,
/// its source's files read from `files`, tracked as the app's, in wave order. The target is
/// `<provider>/aws-test/us-east-1`.
pub fn released(provider: &str, spec: Value, version: &str, files: &[(&str, &str)]) -> anyhow::Result<Vec<Value>> {
    use crate::deploy::model::{App, Contacts, Environment, Package, Registration, ReleaseFile, RepoRef, SCHEMA, Targets};
    let package = Package::Platform { provider: provider.to_owned(), spec, repo: None, path: String::new() };
    let app = App { name: "shop".into(), project: "acme".into(), owners: vec![], package: package.clone(), namespace: None, properties: BTreeMap::new(), template: None, contacts: Contacts::default(), source: None, archived: false, registries: None };
    let mut platforms = BTreeMap::new();
    platforms.insert(provider.to_owned(), json!({ "account": "aws-test", "region": "us-east-1" }).as_object().cloned().unwrap_or_default());
    let env = Environment {
        name: "staging".into(),
        provider: "kilncd".into(),
        release_repo: RepoRef::default(),
        targets: Targets { platforms, ..Targets::default() },
        gates: Default::default(),
        sync: Default::default(),
        watch: Default::default(),
        concurrency: Default::default(),
        agents: Default::default(),
        argocd: Default::default(),
        owners: Default::default(),
    };
    let reg = Registration { app: "shop".into(), environment: "staging".into(), enabled: true, paused: false, running: None, created_at: 0, degraded: None };
    let file = ReleaseFile {
        schema: SCHEMA,
        project: "acme".into(),
        app: "shop".into(),
        environment: "staging".into(),
        release: 1,
        enabled: true,
        version: Some(version.to_owned()),
        package,
        artifacts: BTreeMap::new(),
        requested_by: "ada".into(),
        at: String::new(),
        rollback_of: None,
        secrets: String::new(),
        namespace: None,
        set: BTreeMap::new(),
        helm: None,
    };
    let def = crate::deploy::kilncd::definition(&app, &env, &reg, &file, &format!("c0ffee{version}"), "file:///data/releases.git", false).map_err(|e| anyhow::anyhow!("{e}"))?;
    anyhow::ensure!(def.destination.cluster == format!("{provider}/aws-test/us-east-1"), "the release's target: {}", def.destination.cluster);
    let source = def.sources.first().ok_or_else(|| anyhow::anyhow!("no source"))?;
    let kiln_cd::apps::Engine::Provider(p) = &source.engine else { anyhow::bail!("not a provider's source") };
    let files: Map<String, Value> = files.iter().map(|(k, v)| ((*k).to_owned(), Value::String((*v).to_owned()))).collect();
    let read = |path: &str| files.get(path).and_then(Value::as_str).map(|s| s.as_bytes().to_vec());
    let input = kiln_cd::providers::RenderInput { spec: &p.spec, target: &p.target, app: &def.name, namespace: &def.destination.namespace, revision: &source.revision, read: &read };
    let mut objects: Vec<Value> = kiln_cd::providers::render(provider, &input).map_err(|e| anyhow::anyhow!("{e}"))?.into_iter().map(|o| super::fake::tracked(o, &def.id())).collect();
    objects.sort_by_key(kiln_cd::sync::wave_of);
    Ok(objects)
}
