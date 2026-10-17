//! Resource health, as Argo CD assesses it (gitops-engine `pkg/health`, rule for rule and in
//! the same order), so that an app kiln took over from Argo CD reads the same.
//!
//! [`assess`] tries, in order: the owner's rules for the kind ([`Rules`], where settings and the
//! Lua scripts of Argo CD's resource customizations plug in), then the built-in rule. A
//! terminating object reads `Progressing` ("Pending deletion"), unless it is a hook held by its
//! finalizer. Kinds with no rule have no health (`None`): they don't affect their app's, which is
//! Argo CD's behaviour too. [`generic_conditions`] is kiln's optional rule for custom resources
//! that report a `Ready` condition; it is off unless the owner's rules call it.

use serde_json::Value;

use crate::model::Health;
use crate::obj::{self, as_i64, bool_at, get, i64_at, list_at, str_at};

/// A health status and why.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Status {
    /// The status.
    pub health: Health,
    /// Why, in the words of the rule (may be empty).
    pub message: String,
}

impl Status {
    fn new(health: Health, message: impl Into<String>) -> Option<Status> {
        Some(Status { health, message: message.into() })
    }
}

/// Health rules the owner adds: settings (`resource.customizations.health.<group_kind>`) and Lua
/// scripts. `None` from [`Rules::health`] falls through to the built-in rule.
pub trait Rules {
    /// The health of `obj` by the owner's rule for its kind, or `None` if it has none. `Err` is a
    /// rule that failed: the object reads `Unknown` with the error.
    fn health(&self, obj: &Value) -> Option<Result<Status, String>>;
}

/// No extra rules.
pub struct BuiltIn;

impl Rules for BuiltIn {
    fn health(&self, _obj: &Value) -> Option<Result<Status, String>> {
        None
    }
}

/// The annotation that has a Pod's health ignore its restart policy (Argo CD's).
pub const IGNORE_RESTART_POLICY: &str = "argocd.argoproj.io/ignore-restart-policy";
/// The finalizer that holds a hook until its result is read.
pub const HOOK_FINALIZER: &str = "argocd.argoproj.io/hook-finalizer";

/// The object's health, or `None` if no rule speaks for its kind.
pub fn assess(obj: &Value, rules: &dyn Rules) -> Option<Status> {
    let status = match rules.health(obj) {
        Some(Ok(s)) => Some(s),
        Some(Err(e)) => return Status::new(Health::Unknown, e),
        None => built_in(obj),
    };
    pending_deletion(obj).or(status)
}

fn pending_deletion(obj: &Value) -> Option<Status> {
    get(obj, &["metadata", "deletionTimestamp"]).filter(|v| !v.is_null())?;
    let finalizers: Vec<&str> =
        list_at(obj, &["metadata", "finalizers"]).map(|l| l.iter().filter_map(Value::as_str).collect()).unwrap_or_default();
    if finalizers.contains(&HOOK_FINALIZER) {
        return None;
    }
    let msg = if finalizers.is_empty() {
        "Pending deletion".to_owned()
    } else {
        format!("Pending deletion; blocked by finalizers: {}", finalizers.join(", "))
    };
    Status::new(Health::Progressing, msg)
}

/// The built-in rule for the object's kind.
pub fn built_in(obj: &Value) -> Option<Status> {
    let api_version = str_at(obj, &["apiVersion"]).unwrap_or("");
    let group = obj::api_group(api_version);
    match (group, obj::kind(obj)) {
        ("apps", "Deployment") => Some(versioned(api_version, "apps/v1", "Deployment", deployment(obj))),
        ("apps", "StatefulSet") => Some(versioned(api_version, "apps/v1", "StatefulSet", stateful_set(obj))),
        ("apps", "ReplicaSet") => Some(versioned(api_version, "apps/v1", "ReplicaSet", replica_set(obj))),
        ("apps", "DaemonSet") => Some(versioned(api_version, "apps/v1", "DaemonSet", daemon_set(obj))),
        ("extensions" | "networking.k8s.io", "Ingress") => ingress(obj),
        ("argoproj.io", "Workflow") => argo_workflow(obj),
        ("apiregistration.k8s.io", "APIService") => api_service(obj),
        ("", "Service") => service(obj),
        ("", "PersistentVolumeClaim") => pvc(obj),
        ("", "Pod") => Some(versioned(api_version, "v1", "Pod", pod(obj))),
        ("batch", "Job") => Some(versioned(api_version, "batch/v1", "Job", job(obj))),
        ("autoscaling", "HorizontalPodAutoscaler") => hpa(obj, api_version),
        _ => None,
    }
}

/// Argo CD converts the object to its typed form and refuses other versions.
fn versioned(api_version: &str, want: &str, kind: &str, status: Status) -> Status {
    if api_version == want {
        status
    } else {
        Status { health: Health::Unknown, message: format!("unsupported {kind} GVK: {api_version}") }
    }
}

fn int(obj: &Value, path: &[&str]) -> i64 {
    i64_at(obj, path).unwrap_or(0)
}

fn condition<'a>(obj: &'a Value, type_: &str) -> Option<&'a Value> {
    list_at(obj, &["status", "conditions"])?.iter().find(|c| str_at(c, &["type"]) == Some(type_))
}

fn deployment(obj: &Value) -> Status {
    if bool_at(obj, &["spec", "paused"]).unwrap_or(false) {
        return Status { health: Health::Suspended, message: "Deployment is paused".into() };
    }
    let progressing = |message: String| Status { health: Health::Progressing, message };
    if int(obj, &["metadata", "generation"]) <= int(obj, &["status", "observedGeneration"]) {
        let replicas = i64_at(obj, &["spec", "replicas"]);
        let updated = int(obj, &["status", "updatedReplicas"]);
        let current = int(obj, &["status", "replicas"]);
        let available = int(obj, &["status", "availableReplicas"]);
        let deadline = condition(obj, "Progressing").and_then(|c| str_at(c, &["reason"])) == Some("ProgressDeadlineExceeded");
        if deadline {
            return Status {
                health: Health::Degraded,
                message: format!("Deployment {:?} exceeded its progress deadline", obj::name(obj)),
            };
        }
        if let Some(want) = replicas.filter(|want| updated < *want) {
            return progressing(format!(
                "Waiting for rollout to finish: {updated} out of {want} new replicas have been updated..."
            ));
        }
        if current > updated {
            return progressing(format!(
                "Waiting for rollout to finish: {} old replicas are pending termination...",
                current - updated
            ));
        }
        if available < updated {
            return progressing(format!(
                "Waiting for rollout to finish: {available} of {updated} updated replicas are available..."
            ));
        }
    } else {
        return progressing(
            "Waiting for rollout to finish: observed deployment generation less than desired generation".into(),
        );
    }
    Status { health: Health::Healthy, message: String::new() }
}

fn stateful_set(obj: &Value) -> Status {
    let observed = int(obj, &["status", "observedGeneration"]);
    if observed == 0 || int(obj, &["metadata", "generation"]) > observed {
        return Status { health: Health::Progressing, message: "Waiting for statefulset spec update to be observed...".into() };
    }
    let replicas = i64_at(obj, &["spec", "replicas"]);
    let ready = int(obj, &["status", "readyReplicas"]);
    if let Some(want) = replicas.filter(|want| ready < *want) {
        return Status { health: Health::Progressing, message: format!("Waiting for {} pods to be ready...", want - ready) };
    }
    let strategy = str_at(obj, &["spec", "updateStrategy", "type"]).unwrap_or("RollingUpdate");
    let updated = int(obj, &["status", "updatedReplicas"]);
    if strategy == "RollingUpdate" && get(obj, &["spec", "updateStrategy", "rollingUpdate"]).is_some_and(|v| !v.is_null()) {
        let partition = i64_at(obj, &["spec", "updateStrategy", "rollingUpdate", "partition"]);
        if let (Some(want), Some(partition)) = (replicas, partition)
            && updated < want - partition
        {
            return Status {
                health: Health::Progressing,
                message: format!(
                    "Waiting for partitioned roll out to finish: {updated} out of {} new pods have been updated...",
                    want - partition
                ),
            };
        }
        return Status {
            health: Health::Healthy,
            message: format!("partitioned roll out complete: {updated} new pods have been updated..."),
        };
    }
    if strategy == "OnDelete" {
        return Status { health: Health::Healthy, message: format!("statefulset has {ready} ready pods") };
    }
    let update_rev = str_at(obj, &["status", "updateRevision"]).unwrap_or("");
    let current_rev = str_at(obj, &["status", "currentRevision"]).unwrap_or("");
    if update_rev != current_rev {
        return Status {
            health: Health::Progressing,
            message: format!("waiting for statefulset rolling update to complete {updated} pods at revision {update_rev}..."),
        };
    }
    Status {
        health: Health::Healthy,
        message: format!(
            "statefulset rolling update complete {} pods at revision {current_rev}...",
            int(obj, &["status", "currentReplicas"])
        ),
    }
}

fn daemon_set(obj: &Value) -> Status {
    if int(obj, &["metadata", "generation"]) > int(obj, &["status", "observedGeneration"]) {
        return Status {
            health: Health::Progressing,
            message: "Waiting for rollout to finish: observed daemon set generation less than desired generation".into(),
        };
    }
    let updated = int(obj, &["status", "updatedNumberScheduled"]);
    let desired = int(obj, &["status", "desiredNumberScheduled"]);
    if str_at(obj, &["spec", "updateStrategy", "type"]) == Some("OnDelete") {
        return Status {
            health: Health::Healthy,
            message: format!("daemon set {updated} out of {desired} new pods have been updated"),
        };
    }
    let name = obj::name(obj);
    if updated < desired {
        return Status {
            health: Health::Progressing,
            message: format!(
                "Waiting for daemon set {name:?} rollout to finish: {updated} out of {desired} new pods have been updated..."
            ),
        };
    }
    let available = int(obj, &["status", "numberAvailable"]);
    if available < desired {
        return Status {
            health: Health::Progressing,
            message: format!(
                "Waiting for daemon set {name:?} rollout to finish: {available} of {desired} updated pods are available..."
            ),
        };
    }
    Status { health: Health::Healthy, message: String::new() }
}

fn replica_set(obj: &Value) -> Status {
    if int(obj, &["metadata", "generation"]) <= int(obj, &["status", "observedGeneration"]) {
        if let Some(c) = condition(obj, "ReplicaFailure").filter(|c| str_at(c, &["status"]) == Some("True")) {
            return Status { health: Health::Degraded, message: str_at(c, &["message"]).unwrap_or("").into() };
        }
        let available = int(obj, &["status", "availableReplicas"]);
        if let Some(want) = i64_at(obj, &["spec", "replicas"]).filter(|want| available < *want) {
            return Status {
                health: Health::Progressing,
                message: format!("Waiting for rollout to finish: {available} out of {want} new replicas are available..."),
            };
        }
    } else {
        return Status {
            health: Health::Progressing,
            message: "Waiting for rollout to finish: observed replica set generation less than desired generation".into(),
        };
    }
    Status { health: Health::Healthy, message: String::new() }
}

/// Whether the object is a sync hook (Argo CD's or Helm's annotation).
pub fn is_hook(obj: &Value) -> bool {
    obj::annotation(obj, "argocd.argoproj.io/hook").is_some() || obj::annotation(obj, "helm.sh/hook").is_some()
}

fn pod(obj: &Value) -> Status {
    let ignore_policy = obj::annotation(obj, IGNORE_RESTART_POLICY) == Some("true");
    let hook = is_hook(obj);
    let policy = str_at(obj, &["spec", "restartPolicy"]).unwrap_or("");
    let always = policy == "Always" || (ignore_policy && !hook);
    let message = || str_at(obj, &["status", "message"]).unwrap_or("").to_owned();
    let statuses = list_at(obj, &["status", "containerStatuses"]).map(Vec::as_slice).unwrap_or(&[]);
    if always {
        let mut messages = Vec::new();
        for cs in statuses {
            if let Some(reason) = str_at(cs, &["state", "waiting", "reason"])
                && (reason.starts_with("Err") || reason.ends_with("Error") || reason.ends_with("BackOff"))
            {
                messages.push(str_at(cs, &["state", "waiting", "message"]).unwrap_or("").to_owned());
            }
        }
        if !messages.is_empty() {
            return Status { health: Health::Degraded, message: messages.join(", ") };
        }
    }
    let fail_message = |cs: &Value| -> Option<String> {
        let t = get(cs, &["state", "terminated"])?;
        if let Some(m) = str_at(t, &["message"]).filter(|m| !m.is_empty()) {
            return Some(m.to_owned());
        }
        if str_at(t, &["reason"]) == Some("OOMKilled") {
            return Some("OOMKilled".to_owned());
        }
        let code = i64_at(t, &["exitCode"]).unwrap_or(0);
        (code != 0).then(|| format!("container {:?} failed with exit code {code}", str_at(cs, &["name"]).unwrap_or("")))
    };
    match str_at(obj, &["status", "phase"]).unwrap_or("") {
        "Pending" => Status { health: Health::Progressing, message: message() },
        "Succeeded" => Status { health: Health::Healthy, message: message() },
        "Failed" => {
            let m = message();
            if !m.is_empty() {
                return Status { health: Health::Degraded, message: m };
            }
            let init = list_at(obj, &["status", "initContainerStatuses"]).map(Vec::as_slice).unwrap_or(&[]);
            let found = init.iter().chain(statuses).find_map(fail_message);
            Status { health: Health::Degraded, message: found.unwrap_or_default() }
        }
        "Running" => {
            if always {
                let ready = condition(obj, "Ready").and_then(|c| str_at(c, &["status"])) == Some("True");
                if ready {
                    return Status { health: Health::Healthy, message: message() };
                }
                if statuses.iter().any(|cs| get(cs, &["lastState", "terminated"]).is_some_and(|t| !t.is_null())) {
                    return Status { health: Health::Degraded, message: message() };
                }
                Status { health: Health::Progressing, message: message() }
            } else if policy == "OnFailure" || policy == "Never" {
                Status { health: Health::Progressing, message: message() }
            } else {
                Status { health: Health::Unknown, message: message() }
            }
        }
        _ => Status { health: Health::Unknown, message: message() },
    }
}

fn job(obj: &Value) -> Status {
    let (mut failed, mut complete, mut suspended) = (false, false, false);
    let (mut fail_msg, mut message) = (String::new(), String::new());
    for c in list_at(obj, &["status", "conditions"]).map(Vec::as_slice).unwrap_or(&[]) {
        let text = str_at(c, &["message"]).unwrap_or("").to_owned();
        match str_at(c, &["type"]) {
            Some("Failed") => {
                failed = true;
                complete = true;
                fail_msg = text;
            }
            Some("Complete") => {
                complete = true;
                message = text;
            }
            Some("Suspended") => {
                complete = true;
                message = text;
                if str_at(c, &["status"]) == Some("True") {
                    suspended = true;
                }
            }
            _ => {}
        }
    }
    match () {
        () if !complete => Status { health: Health::Progressing, message },
        () if failed => Status { health: Health::Degraded, message: fail_msg },
        () if suspended => Status { health: Health::Suspended, message },
        () => Status { health: Health::Healthy, message },
    }
}

fn pvc(obj: &Value) -> Option<Status> {
    let health = match str_at(obj, &["status", "phase"]) {
        Some("Lost") => Health::Degraded,
        Some("Pending") => Health::Progressing,
        Some("Bound") => Health::Healthy,
        _ => Health::Unknown,
    };
    Status::new(health, "")
}

fn service(obj: &Value) -> Option<Status> {
    if str_at(obj, &["spec", "type"]) == Some("LoadBalancer") {
        let ingress = list_at(obj, &["status", "loadBalancer", "ingress"]).is_some_and(|l| !l.is_empty());
        return Status::new(if ingress { Health::Healthy } else { Health::Progressing }, "");
    }
    Status::new(Health::Healthy, "")
}

fn ingress(obj: &Value) -> Option<Status> {
    let ingress = list_at(obj, &["status", "loadBalancer", "ingress"]).is_some_and(|l| !l.is_empty());
    Status::new(if ingress { Health::Healthy } else { Health::Progressing }, "")
}

fn argo_workflow(obj: &Value) -> Option<Status> {
    let message = str_at(obj, &["status", "message"]).unwrap_or("");
    let health = match str_at(obj, &["status", "phase"]).unwrap_or("") {
        "" | "Pending" | "Running" => Health::Progressing,
        "Succeeded" => Health::Healthy,
        "Failed" | "Error" => Health::Degraded,
        _ => Health::Unknown,
    };
    Status::new(health, message)
}

fn api_service(obj: &Value) -> Option<Status> {
    match str_at(obj, &["apiVersion"]) {
        Some("apiregistration.k8s.io/v1" | "apiregistration.k8s.io/v1beta1") => {}
        Some(other) => return Status::new(Health::Unknown, format!("unsupported APIService GVK: {other}")),
        None => return None,
    }
    if let Some(c) = condition(obj, "Available") {
        let message = format!("{}: {}", str_at(c, &["reason"]).unwrap_or(""), str_at(c, &["message"]).unwrap_or(""));
        let health = if str_at(c, &["status"]) == Some("True") { Health::Healthy } else { Health::Progressing };
        return Status::new(health, message);
    }
    Status::new(Health::Progressing, "Waiting to be processed")
}

const HPA_DEGRADED: &[(&str, &str)] = &[
    ("AbleToScale", "FailedGetScale"),
    ("AbleToScale", "FailedUpdateScale"),
    ("ScalingActive", "FailedGetResourceMetric"),
    ("ScalingActive", "FailedGetObjectMetric"),
    ("ScalingActive", "FailedGetPodsMetric"),
    ("ScalingActive", "FailedGetExternalMetric"),
    ("ScalingActive", "FailedComputeMetricsReplicas"),
    ("ScalingActive", "InvalidSelector"),
];

fn hpa(obj: &Value, api_version: &str) -> Option<Status> {
    let waiting = || Status::new(Health::Progressing, "Waiting to Autoscale");
    let parsed: Vec<Value>;
    let conditions: &[Value] = match api_version {
        "autoscaling/v2" => list_at(obj, &["status", "conditions"]).map(Vec::as_slice).unwrap_or(&[]),
        "autoscaling/v1" => {
            let Some(text) = obj::annotation(obj, "autoscaling.alpha.kubernetes.io/conditions") else {
                return waiting();
            };
            match serde_json::from_str::<Vec<Value>>(text) {
                Ok(list) => {
                    parsed = list;
                    &parsed
                }
                Err(e) => return Status::new(Health::Unknown, format!("failed to parse HPA conditions: {e}")),
            }
        }
        other => return Status::new(Health::Unknown, format!("unsupported HPA GVK: {other}")),
    };
    let field = |c: &Value, k: &str| str_at(c, &[k]).unwrap_or("").to_owned();
    for c in conditions {
        let (t, r) = (field(c, "type"), field(c, "reason"));
        if HPA_DEGRADED.iter().any(|(dt, dr)| *dt == t && *dr == r) {
            return Status::new(Health::Degraded, field(c, "message"));
        }
    }
    for c in conditions {
        let t = field(c, "type");
        if (t == "AbleToScale" || t == "ScalingLimited") && field(c, "status") == "True" {
            return Status::new(Health::Healthy, field(c, "message"));
        }
    }
    waiting()
}

/// kiln's rule for custom resources that report a `Ready` (or `Available`) condition: `True` is
/// healthy; `False` is degraded when its reason says it failed, progressing otherwise; no such
/// condition yet is progressing once the controller has seen the object (`observedGeneration`),
/// and no health at all for kinds that never report one.
pub fn generic_conditions(obj: &Value) -> Option<Status> {
    let c = condition(obj, "Ready").or_else(|| condition(obj, "Available"))?;
    let message = str_at(c, &["message"]).unwrap_or("").to_owned();
    if let Some(observed) = i64_at(obj, &["status", "observedGeneration"])
        && as_i64(get(obj, &["metadata", "generation"]).unwrap_or(&Value::Null)).is_some_and(|g| g > observed)
    {
        return Status::new(Health::Progressing, "Waiting for the controller to observe the latest spec");
    }
    match str_at(c, &["status"]) {
        Some("True") => Status::new(Health::Healthy, message),
        Some("False") => {
            let reason = str_at(c, &["reason"]).unwrap_or("").to_ascii_lowercase();
            let failed = ["fail", "error", "invalid", "denied", "backoff"].iter().any(|w| reason.contains(w));
            Status::new(if failed { Health::Degraded } else { Health::Progressing }, message)
        }
        _ => Status::new(Health::Progressing, message),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn h(v: Value) -> Option<Health> {
        assess(&v, &BuiltIn).map(|s| s.health)
    }

    #[test]
    fn deployments_roll_out_then_turn_healthy() {
        let base = |status: Value| {
            json!({"apiVersion": "apps/v1", "kind": "Deployment", "metadata": {"name": "web", "generation": 2},
                "spec": {"replicas": 3}, "status": status})
        };
        assert_eq!(h(base(json!({"observedGeneration": 1}))), Some(Health::Progressing));
        assert_eq!(h(base(json!({"observedGeneration": 2, "replicas": 3, "updatedReplicas": 1}))), Some(Health::Progressing));
        assert_eq!(
            h(base(json!({"observedGeneration": 2, "replicas": 3, "updatedReplicas": 3, "availableReplicas": 3}))),
            Some(Health::Healthy)
        );
        assert_eq!(
            h(base(json!({"observedGeneration": 2, "conditions": [{"type": "Progressing", "reason": "ProgressDeadlineExceeded"}]}))),
            Some(Health::Degraded)
        );
        let paused = json!({"apiVersion": "apps/v1", "kind": "Deployment", "metadata": {"name": "w"}, "spec": {"paused": true}});
        assert_eq!(h(paused), Some(Health::Suspended));
    }

    #[test]
    fn pods_degrade_on_crash_loops_and_terminated_restarts() {
        let crash = json!({"apiVersion": "v1", "kind": "Pod", "metadata": {"name": "p"}, "spec": {"restartPolicy": "Always"},
            "status": {"phase": "Running", "containerStatuses": [{"name": "a", "state": {"waiting": {"reason": "CrashLoopBackOff", "message": "back-off"}}}]}});
        assert_eq!(assess(&crash, &BuiltIn).map(|s| (s.health, s.message)), Some((Health::Degraded, "back-off".into())));
        let ready = json!({"apiVersion": "v1", "kind": "Pod", "metadata": {"name": "p"}, "spec": {"restartPolicy": "Always"},
            "status": {"phase": "Running", "conditions": [{"type": "Ready", "status": "True"}]}});
        assert_eq!(h(ready), Some(Health::Healthy));
        let job_pod = json!({"apiVersion": "v1", "kind": "Pod", "metadata": {"name": "p"}, "spec": {"restartPolicy": "Never"},
            "status": {"phase": "Running"}});
        assert_eq!(h(job_pod), Some(Health::Progressing));
    }

    #[test]
    fn jobs_count_conditions_whatever_their_status() {
        let failed = json!({"apiVersion": "batch/v1", "kind": "Job", "metadata": {"name": "j"},
            "status": {"conditions": [{"type": "Failed", "status": "True", "message": "BackoffLimitExceeded"}]}});
        assert_eq!(h(failed), Some(Health::Degraded));
        let running = json!({"apiVersion": "batch/v1", "kind": "Job", "metadata": {"name": "j"}, "status": {"active": 1}});
        assert_eq!(h(running), Some(Health::Progressing));
    }

    #[test]
    fn terminating_objects_are_pending_deletion_unless_hooks_hold_them() {
        let cm = json!({"apiVersion": "v1", "kind": "Service", "metadata": {"name": "s", "deletionTimestamp": "2026-10-16T00:00:00Z", "finalizers": ["x/y"]}});
        assert_eq!(
            assess(&cm, &BuiltIn).map(|s| s.message),
            Some("Pending deletion; blocked by finalizers: x/y".into())
        );
        let hook = json!({"apiVersion": "batch/v1", "kind": "Job", "metadata": {"name": "j", "deletionTimestamp": "t", "finalizers": [HOOK_FINALIZER]},
            "status": {"conditions": [{"type": "Complete"}]}});
        assert_eq!(h(hook), Some(Health::Healthy));
    }

    #[test]
    fn unknown_kinds_have_no_health_and_other_versions_are_refused() {
        assert_eq!(h(json!({"apiVersion": "example.com/v1", "kind": "Widget", "metadata": {"name": "w"}})), None);
        assert_eq!(h(json!({"apiVersion": "apps/v1beta2", "kind": "Deployment", "metadata": {"name": "w"}})), Some(Health::Unknown));
        let ready = json!({"apiVersion": "example.com/v1", "kind": "Widget", "metadata": {"name": "w"},
            "status": {"conditions": [{"type": "Ready", "status": "False", "reason": "ReconcileFailed"}]}});
        assert_eq!(generic_conditions(&ready).map(|s| s.health), Some(Health::Degraded));
    }

    #[test]
    fn hpas_read_v1_annotations_and_v2_conditions() {
        let v2 = json!({"apiVersion": "autoscaling/v2", "kind": "HorizontalPodAutoscaler", "metadata": {"name": "a"},
            "status": {"conditions": [{"type": "AbleToScale", "status": "True", "message": "ok"}]}});
        assert_eq!(h(v2), Some(Health::Healthy));
        let v1 = json!({"apiVersion": "autoscaling/v1", "kind": "HorizontalPodAutoscaler", "metadata": {"name": "a",
            "annotations": {"autoscaling.alpha.kubernetes.io/conditions": "[{\"type\":\"ScalingActive\",\"reason\":\"FailedGetResourceMetric\",\"message\":\"no metrics\"}]"}}});
        assert_eq!(h(v1), Some(Health::Degraded));
    }
}
