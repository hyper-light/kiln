//! `aws-ecs` (docs/cd-targets.md §3): a task definition and the service that runs it.
//!
//! ```yaml
//! api:
//!   aws-ecs:
//!     cluster: main
//!     service: api
//!     task: deploy/api/task.json          # RegisterTaskDefinition's input
//!     service-spec: deploy/api/svc.json   # CreateService's input (desiredCount, networkConfiguration, …)
//!     set:
//!       "containerDefinitions[name=api].image": "${{ artifacts.image }}"
//!       "service.desiredCount": 3
//! ```
//!
//! - **TaskDefinition** (its family, outside any ECS cluster): `spec` is RegisterTaskDefinition's
//!   input. Task definitions are immutable revisions, so applying one registers a new revision
//!   when the newest one differs; the object reads as the family's newest active revision.
//! - **Service** (in its ECS cluster, the destination's namespace): `spec` is CreateService's
//!   input, with `taskDefinition` the family: the service runs the family's newest revision. A
//!   service on any other revision reads as that revision's ARN: drift, or a deployment rolled
//!   back by hand.
//!
//! Fields kiln doesn't set belong to others: leave `desiredCount` out of the service's spec when
//! Application Auto Scaling owns it.

use serde_json::{Map, Value, json};

use super::{RenderInput, apply_sets, file_or_object, file_settings, object, status, text};
use crate::health::Status;
use crate::model::Health;

pub const GROUP: &str = "ecs.aws.kiln.dev";

/// What ECS reports of a task definition that isn't part of it.
const TASK_READ_ONLY: &[&str] = &["taskDefinitionArn", "revision", "status", "requiresAttributes", "compatibilities", "registeredAt", "registeredBy", "deregisteredAt", "deleteRequestedAt"];

/// What ECS reports of a service that isn't part of its settings.
const SERVICE_READ_ONLY: &[&str] = &[
    "serviceArn",
    "serviceName",
    "clusterArn",
    "cluster",
    "status",
    "runningCount",
    "pendingCount",
    "deployments",
    "taskSets",
    "events",
    "createdAt",
    "createdBy",
    "roleArn",
    "taskDefinition",
    "currentServiceDeployment",
    "currentServiceRevisions",
    "resourceManagementType",
];

/// Settings UpdateService takes.
pub const UPDATABLE: &[&str] = &[
    "desiredCount",
    "capacityProviderStrategy",
    "deploymentConfiguration",
    "availabilityZoneRebalancing",
    "networkConfiguration",
    "placementConstraints",
    "placementStrategy",
    "platformVersion",
    "healthCheckGracePeriodSeconds",
    "deploymentController",
    "enableExecuteCommand",
    "enableECSManagedTags",
    "loadBalancers",
    "propagateTags",
    "serviceRegistries",
    "serviceConnectConfiguration",
    "volumeConfigurations",
    "vpcLatticeConfigurations",
];

/// The files a render reads.
pub fn files(spec: &Value) -> Vec<String> {
    file_settings(spec, &["task", "service-spec"])
}

/// An app's objects.
pub fn render(input: &RenderInput<'_>) -> Result<Vec<Value>, String> {
    let cluster = text(input.spec, "cluster").map(str::to_owned).or_else(|| (!input.namespace.is_empty()).then(|| input.namespace.to_owned())).ok_or("aws-ecs needs cluster: the ECS cluster")?;
    let service = text(input.spec, "service").ok_or("aws-ecs needs service: the service's name")?;
    let mut task = match file_or_object(input, "task")? {
        Some(Value::Object(m)) => Value::Object(m),
        Some(_) => return Err("task: an object (RegisterTaskDefinition's input)".into()),
        None => return Err("aws-ecs needs task: the task definition, a JSON file or an object".into()),
    };
    // A definition copied from describe-task-definition carries what ECS adds; drop it.
    if let Value::Object(m) = &mut task {
        if let Some(Value::Object(inner)) = m.remove("taskDefinition") {
            *m = inner;
        }
        for k in TASK_READ_ONLY {
            m.remove(*k);
        }
    }
    let mut svc = match file_or_object(input, "service-spec")? {
        Some(Value::Object(m)) => Value::Object(m),
        Some(_) => return Err("service-spec: an object (CreateService's input)".into()),
        None => Value::Object(Map::new()),
    };
    if let Value::Object(m) = &mut svc {
        for k in SERVICE_READ_ONLY {
            m.remove(*k);
        }
        m.remove("service");
    }
    // `set:` paths under `service.` are the service's; the rest the task definition's.
    if let Some(Value::Object(sets)) = input.spec.get("set") {
        let (svc_sets, task_sets): (Map<String, Value>, Map<String, Value>) = sets.iter().map(|(k, v)| (k.clone(), v.clone())).partition(|(k, _)| k.starts_with("service."));
        let svc_sets: Map<String, Value> = svc_sets.into_iter().map(|(k, v)| (k.trim_start_matches("service.").to_owned(), v)).collect();
        apply_sets(&mut task, Some(&Value::Object(task_sets)))?;
        apply_sets(&mut svc, Some(&Value::Object(svc_sets)))?;
    }
    let family = task.get("family").and_then(Value::as_str).filter(|f| !f.is_empty()).unwrap_or(service).to_owned();
    if let Value::Object(m) = &mut task {
        m.insert("family".into(), Value::String(family.clone()));
    }
    normalize_task(&mut task);
    if task.get("containerDefinitions").and_then(Value::as_array).is_none_or(Vec::is_empty) {
        return Err(format!("task definition {family} has no containerDefinitions"));
    }
    if let Value::Object(m) = &mut svc {
        m.insert("taskDefinition".into(), Value::String(family.clone()));
    }
    Ok(vec![wave(object(GROUP, "TaskDefinition", &family, None, task), 0), wave(object(GROUP, "Service", service, Some(&cluster), svc), 1)])
}

fn wave(mut o: Value, w: i64) -> Value {
    if let Some(ann) = crate::obj::ensure_map(&mut o, &["metadata", "annotations"]) {
        ann.insert(crate::sync::KILN_WAVE.into(), Value::String(w.to_string()));
    }
    o
}

/// The spellings ECS keeps, so a definition compares as ECS reports it: task-level `cpu` and
/// `memory` are strings; containers' environment and secrets in name order.
pub fn normalize_task(task: &mut Value) {
    let Value::Object(m) = task else { return };
    for k in ["cpu", "memory"] {
        if let Some(Value::Number(n)) = m.get(k).cloned() {
            m.insert(k.to_owned(), Value::String(n.to_string()));
        }
    }
    if let Some(Value::Array(cs)) = m.get_mut("containerDefinitions") {
        for c in cs.iter_mut() {
            for list in ["environment", "secrets"] {
                if let Some(Value::Array(items)) = c.get_mut(list) {
                    items.sort_by(|a, b| a.get("name").and_then(Value::as_str).cmp(&b.get("name").and_then(Value::as_str)));
                }
            }
        }
    }
}

// ------------------------------------------------------------------ ECS's answers → objects

/// A TaskDefinition object from DescribeTaskDefinition's answer (`include: [TAGS]`).
pub fn live_task_definition(answer: &Value) -> Option<Value> {
    let td = answer.get("taskDefinition")?;
    let family = td.get("family")?.as_str()?;
    let mut spec = td.as_object()?.clone();
    for k in TASK_READ_ONLY {
        spec.remove(*k);
    }
    let mut spec = Value::Object(spec);
    let tags = tags_map(answer.get("tags"));
    let user: Vec<Value> = tags.iter().filter(|(k, _)| !k.starts_with("kiln.dev/")).map(|(k, v)| json!({ "key": k, "value": v })).collect();
    if let (false, Value::Object(m)) = (user.is_empty(), &mut spec) {
        m.insert("tags".into(), Value::Array(user));
    }
    normalize_task(&mut spec);
    let mut o = object(GROUP, "TaskDefinition", family, None, spec);
    if let Value::Object(m) = &mut o {
        m.insert("status".into(), json!({ "taskDefinitionArn": td.get("taskDefinitionArn"), "revision": td.get("revision"), "status": td.get("status") }));
    }
    tracking(&mut o, &tags);
    Some(o)
}

/// A Service object from one of DescribeServices' services (`include: [TAGS]`). `newest` is
/// the ARN of its family's newest active revision.
pub fn live_service(svc: &Value, newest: Option<&str>) -> Option<Value> {
    let name = svc.get("serviceName")?.as_str()?;
    let cluster = svc.get("clusterArn").and_then(Value::as_str).and_then(|a| a.rsplit('/').next()).unwrap_or_default();
    let td = svc.get("taskDefinition").and_then(Value::as_str).unwrap_or_default();
    let mut spec = svc.as_object()?.clone();
    for k in SERVICE_READ_ONLY {
        spec.remove(*k);
    }
    spec.remove("tags");
    let shown = if newest == Some(td) { family_of(td).unwrap_or(td).to_owned() } else { td.to_owned() };
    spec.insert("taskDefinition".into(), Value::String(shown));
    let tags = tags_map(svc.get("tags"));
    let user: Vec<Value> = tags.iter().filter(|(k, _)| !k.starts_with("kiln.dev/")).map(|(k, v)| json!({ "key": k, "value": v })).collect();
    if !user.is_empty() {
        spec.insert("tags".into(), Value::Array(user));
    }
    let mut o = object(GROUP, "Service", name, Some(cluster), Value::Object(spec));
    if let Value::Object(m) = &mut o {
        let deployments: Vec<Value> = svc
            .get("deployments")
            .and_then(Value::as_array)
            .map(|d| d.iter().map(|x| json!({ "id": x.get("id"), "status": x.get("status"), "taskDefinition": x.get("taskDefinition"), "desiredCount": x.get("desiredCount"), "runningCount": x.get("runningCount"), "rolloutState": x.get("rolloutState"), "rolloutStateReason": x.get("rolloutStateReason") })).collect())
            .unwrap_or_default();
        let events: Vec<Value> = svc.get("events").and_then(Value::as_array).map(|e| e.iter().take(10).cloned().collect()).unwrap_or_default();
        m.insert(
            "status".into(),
            json!({ "status": svc.get("status"), "desiredCount": svc.get("desiredCount"), "runningCount": svc.get("runningCount"), "pendingCount": svc.get("pendingCount"), "taskDefinition": td, "deployments": deployments, "events": events, "serviceArn": svc.get("serviceArn") }),
        );
    }
    tracking(&mut o, &tags);
    Some(o)
}

/// A task definition ARN's family: `arn:aws:ecs:…:task-definition/api:42` → `api`.
pub fn family_of(arn: &str) -> Option<&str> {
    arn.rsplit_once("task-definition/").map(|(_, r)| r).and_then(|r| r.rsplit_once(':')).map(|(f, _)| f)
}

/// ECS tags (`[{key, value}]`) as a map.
pub fn tags_map(tags: Option<&Value>) -> Map<String, Value> {
    tags.and_then(Value::as_array)
        .map(|t| t.iter().filter_map(|x| Some((x.get("key")?.as_str()?.to_owned(), x.get("value").cloned().unwrap_or(Value::String(String::new()))))).collect())
        .unwrap_or_default()
}

fn tracking(o: &mut Value, tags: &Map<String, Value>) {
    super::aws_lambda::tracking_from_tags(o, Some(&Value::Object(tags.clone())));
}

/// The tags an object is applied with, as ECS takes them: its own (`spec.tags`) and kiln's.
pub fn tags_of(desired: &Value) -> Vec<Value> {
    let mut all = tags_map(desired.pointer("/spec/tags"));
    for (k, v) in super::aws_lambda::tags_of(&strip_spec_tags(desired)) {
        all.insert(k, v);
    }
    all.into_iter().map(|(k, v)| json!({ "key": k, "value": v })).collect()
}

fn strip_spec_tags(desired: &Value) -> Value {
    let mut d = desired.clone();
    if let Some(s) = d.get_mut("spec").and_then(Value::as_object_mut) {
        s.remove("Tags");
    }
    d
}

// ------------------------------------------------------------------ objects → ECS's requests

/// RegisterTaskDefinition's body.
pub fn register_body(desired: &Value) -> Value {
    let mut b = desired.get("spec").and_then(Value::as_object).cloned().unwrap_or_default();
    b.insert("tags".into(), Value::Array(tags_of(desired)));
    Value::Object(b)
}

/// CreateService's body, running `task_definition` (an ARN).
pub fn create_service_body(desired: &Value, task_definition: &str) -> Value {
    let mut b = desired.get("spec").and_then(Value::as_object).cloned().unwrap_or_default();
    b.insert("cluster".into(), Value::String(crate::obj::namespace(desired).to_owned()));
    b.insert("serviceName".into(), Value::String(crate::obj::name(desired).to_owned()));
    b.insert("taskDefinition".into(), Value::String(task_definition.to_owned()));
    b.insert("tags".into(), Value::Array(tags_of(desired)));
    b.entry("desiredCount").or_insert(json!(1));
    b.entry("propagateTags").or_insert(json!("SERVICE"));
    Value::Object(b)
}

/// UpdateService's body: the settings kiln sets that UpdateService takes, running
/// `task_definition` (an ARN).
pub fn update_service_body(desired: &Value, task_definition: &str) -> Value {
    let spec = desired.get("spec").cloned().unwrap_or(Value::Null);
    let mut b: Map<String, Value> = UPDATABLE.iter().filter_map(|k| spec.get(*k).map(|v| ((*k).to_owned(), v.clone()))).collect();
    b.insert("cluster".into(), Value::String(crate::obj::namespace(desired).to_owned()));
    b.insert("service".into(), Value::String(crate::obj::name(desired).to_owned()));
    b.insert("taskDefinition".into(), Value::String(task_definition.to_owned()));
    Value::Object(b)
}

/// Settings of the service a change can't make in place (CreateService only): changing them
/// means a new service.
pub fn immutable_changes(desired: &Value, live: &Value) -> Vec<&'static str> {
    ["launchType", "schedulingStrategy", "role"]
        .into_iter()
        .filter(|k| match desired.pointer(&format!("/spec/{k}")) {
            None => false,
            Some(d) => live.pointer(&format!("/spec/{k}")).is_none_or(|l| !super::aws_lambda::covers(d, l)),
        })
        .collect()
}

/// Whether the family's newest revision already is what kiln wants (so no new revision).
pub fn task_matches(desired: &Value, live: &Value) -> bool {
    let (Some(d), Some(l)) = (desired.get("spec"), live.get("spec")) else { return false };
    let mut d = strip_spec_tags(&json!({ "spec": d })).get("spec").cloned().unwrap_or(Value::Null);
    if let Some(m) = d.as_object_mut() {
        m.remove("tags");
    }
    super::aws_lambda::covers(&d, l)
}

// ------------------------------------------------------------------ health

pub fn health(obj: &Value) -> Option<Status> {
    match crate::obj::kind(obj) {
        "TaskDefinition" => match crate::obj::str_at(obj, &["status", "status"]) {
            Some("INACTIVE") | Some("DELETE_IN_PROGRESS") => status(Health::Degraded, "deregistered"),
            _ => status(Health::Healthy, ""),
        },
        "Service" => service_health(obj),
        "Task" => match crate::obj::str_at(obj, &["status", "lastStatus"]) {
            Some("RUNNING") => status(Health::Healthy, ""),
            Some("STOPPED") => status(Health::Degraded, crate::obj::str_at(obj, &["status", "stoppedReason"]).unwrap_or("stopped")),
            Some(s) => status(Health::Progressing, s.to_ascii_lowercase()),
            None => None,
        },
        _ => None,
    }
}

fn service_health(obj: &Value) -> Option<Status> {
    let st = |k: &str| obj.pointer(&format!("/status/{k}"));
    match st("status").and_then(Value::as_str) {
        Some("DRAINING") => return status(Health::Progressing, "draining"),
        Some("INACTIVE") => return status(Health::Missing, "deleted"),
        _ => {}
    }
    let deployments = st("deployments").and_then(Value::as_array).cloned().unwrap_or_default();
    let primary = deployments.iter().find(|d| d.get("status").and_then(Value::as_str) == Some("PRIMARY"));
    if let Some(p) = primary {
        let reason = p.get("rolloutStateReason").and_then(Value::as_str).unwrap_or_default();
        match p.get("rolloutState").and_then(Value::as_str) {
            Some("FAILED") => return status(Health::Degraded, reason),
            Some("IN_PROGRESS") => return status(Health::Progressing, if reason.is_empty() { "deploying" } else { reason }),
            _ => {}
        }
    }
    if deployments.len() > 1 {
        return status(Health::Progressing, "draining the previous deployment");
    }
    let (want, running) = (st("desiredCount").and_then(Value::as_i64).unwrap_or(0), st("runningCount").and_then(Value::as_i64).unwrap_or(0));
    if running < want {
        return status(Health::Progressing, format!("{running} of {want} tasks running"));
    }
    status(Health::Healthy, format!("{running} of {want} tasks running"))
}

#[cfg(test)]
mod tests {
    use super::*;

    const TASK: &str = r#"{
        "family": "api", "cpu": 256, "memory": "512", "networkMode": "awsvpc", "requiresCompatibilities": ["FARGATE"],
        "executionRoleArn": "arn:aws:iam::1:role/exec",
        "containerDefinitions": [{ "name": "api", "image": "placeholder", "portMappings": [{ "containerPort": 8080 }],
          "environment": [{ "name": "Z", "value": "1" }, { "name": "A", "value": "2" }] }],
        "revision": 41, "status": "ACTIVE"
    }"#;

    fn render_api(extra: Value) -> Result<Vec<Value>, String> {
        let mut spec = json!({ "cluster": "main", "service": "api", "task": "task.json", "set": { "containerDefinitions[name=api].image": "acme/api@sha256:abc", "service.desiredCount": 3 } });
        if let (Value::Object(m), Value::Object(e)) = (&mut spec, extra) {
            m.extend(e);
        }
        let read = |p: &str| (p == "task.json").then(|| TASK.as_bytes().to_vec());
        let none = Map::new();
        render(&RenderInput { spec: &spec, target: &none, app: "api", namespace: "", revision: "abc", read: &read })
    }

    #[test]
    fn a_definition_renders_a_task_definition_and_its_service() -> Result<(), String> {
        let objs = render_api(json!({}))?;
        let td = objs.first().ok_or("no task definition")?;
        assert_eq!((crate::obj::name(td), crate::obj::namespace(td)), ("api", ""));
        assert_eq!(td.pointer("/spec/containerDefinitions/0/image"), Some(&json!("acme/api@sha256:abc")));
        assert_eq!(td.pointer("/spec/cpu"), Some(&json!("256")), "task-level cpu is a string, as ECS keeps it");
        assert_eq!(td.pointer("/spec/containerDefinitions/0/environment/0/name"), Some(&json!("A")));
        assert_eq!(td.pointer("/spec/revision"), None, "what ECS adds is dropped");
        let svc = objs.get(1).ok_or("no service")?;
        assert_eq!((crate::obj::name(svc), crate::obj::namespace(svc)), ("api", "main"));
        assert_eq!(svc.pointer("/spec/taskDefinition"), Some(&json!("api")));
        assert_eq!(svc.pointer("/spec/desiredCount"), Some(&json!(3)));
        assert!(render_api(json!({ "task": { "family": "x" } })).is_err_and(|e| e.contains("containerDefinitions")));
        Ok(())
    }

    #[test]
    fn ecs_answers_read_as_objects() -> Result<(), String> {
        let objs = render_api(json!({}))?;
        let (td, svc) = (objs.first().ok_or("td")?, objs.get(1).ok_or("svc")?);
        let mut registered = td.get("spec").cloned().unwrap_or(Value::Null);
        if let Value::Object(m) = &mut registered {
            m.insert("taskDefinitionArn".into(), json!("arn:aws:ecs:us-east-1:1:task-definition/api:42"));
            m.insert("revision".into(), json!(42));
            m.insert("status".into(), json!("ACTIVE"));
            m.insert("compatibilities".into(), json!(["EC2", "FARGATE"]));
            if let Some(Value::Array(cs)) = m.get_mut("containerDefinitions")
                && let Some(Value::Object(c)) = cs.first_mut()
            {
                // What ECS fills in.
                c.insert("essential".into(), json!(true));
                c.insert("portMappings".into(), json!([{ "containerPort": 8080, "hostPort": 8080, "protocol": "tcp" }]));
                c.insert("environment".into(), json!([{ "name": "Z", "value": "1" }, { "name": "A", "value": "2" }]));
            }
        }
        let live_td = live_task_definition(&json!({ "taskDefinition": registered, "tags": [{ "key": "kiln.dev/app", "value": "00ff" }, { "key": "team", "value": "api" }] })).ok_or("no live td")?;
        assert!(task_matches(td, &live_td), "ECS's additions don't count as a change");
        assert_eq!(crate::obj::label(&live_td, crate::engine::APP_LABEL), Some("00ff"));
        assert_eq!(live_td.pointer("/spec/tags"), Some(&json!([{ "key": "team", "value": "api" }])));
        let mut changed = td.clone();
        crate::providers::set_path(&mut changed, "spec.containerDefinitions[name=api].image", json!("acme/api@sha256:def"))?;
        assert!(!task_matches(&changed, &live_td));

        let arn = "arn:aws:ecs:us-east-1:1:task-definition/api:42";
        let answer = json!({ "serviceName": "api", "clusterArn": "arn:aws:ecs:us-east-1:1:cluster/main", "taskDefinition": arn, "desiredCount": 3, "runningCount": 3, "status": "ACTIVE",
            "deployments": [{ "id": "d1", "status": "PRIMARY", "rolloutState": "COMPLETED", "taskDefinition": arn }], "launchType": "FARGATE" });
        let live = live_service(&answer, Some(arn)).ok_or("no service")?;
        assert_eq!((crate::obj::name(&live), crate::obj::namespace(&live)), ("api", "main"));
        assert_eq!(live.pointer("/spec/taskDefinition"), Some(&json!("api")));
        assert_eq!(health(&live).map(|s| s.health), Some(Health::Healthy));
        // On another revision: its ARN, which differs from the family kiln wants.
        let other = live_service(&answer, Some("arn:aws:ecs:us-east-1:1:task-definition/api:43")).ok_or("no service")?;
        assert_eq!(other.pointer("/spec/taskDefinition"), Some(&json!(arn)));
        let body = update_service_body(svc, "arn:new");
        assert_eq!((body.get("service"), body.get("cluster"), body.get("desiredCount"), body.get("taskDefinition")), (Some(&json!("api")), Some(&json!("main")), Some(&json!(3)), Some(&json!("arn:new"))));
        assert!(immutable_changes(svc, &live).is_empty());
        assert_eq!(family_of(arn), Some("api"));
        Ok(())
    }

    #[test]
    fn service_health_follows_its_deployments() {
        let svc = |rollout: &str, running: i64, n: usize| {
            let mut deployments = vec![json!({ "status": "PRIMARY", "rolloutState": rollout, "rolloutStateReason": "tasks failed to start" })];
            if n > 1 {
                deployments.push(json!({ "status": "ACTIVE" }));
            }
            json!({ "apiVersion": "ecs.aws.kiln.dev/v1", "kind": "Service", "status": { "status": "ACTIVE", "desiredCount": 2, "runningCount": running, "deployments": deployments } })
        };
        assert_eq!(health(&svc("COMPLETED", 2, 1)).map(|s| s.health), Some(Health::Healthy));
        assert_eq!(health(&svc("COMPLETED", 1, 1)).map(|s| s.health), Some(Health::Progressing));
        assert_eq!(health(&svc("IN_PROGRESS", 2, 2)).map(|s| s.health), Some(Health::Progressing));
        assert_eq!(health(&svc("FAILED", 0, 1)).map(|s| (s.health, s.message)), Some((Health::Degraded, "tasks failed to start".to_owned())));
        assert_eq!(health(&svc("COMPLETED", 2, 2)).map(|s| s.health), Some(Health::Progressing));
    }
}
