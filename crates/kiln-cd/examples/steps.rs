//! What one live event costs, step by step, on a Deployment as the API server returns it.
//! `cargo run --release -p kiln-cd --example steps`

use std::hint::black_box;
use std::time::Instant;

use kiln_cd::diff::{self, LiveView};
use kiln_cd::health;
use kiln_cd::obj::{self, Blob};
use serde_json::{Value, json};

fn live_deployment() -> Value {
    json!({"apiVersion": "apps/v1", "kind": "Deployment", "metadata": {"name": "app-1", "namespace": "ns-1", "uid": "8b0e2c9a-5f7a-4d5e-9c3a-1f2e3d4c5b6a",
        "resourceVersion": "123456789", "generation": 4, "creationTimestamp": "2026-10-16T00:00:00Z",
        "labels": {"app": "app-1", "team": "t", "kiln.dev/app": "0123456789abcdef"},
        "annotations": {"kiln.dev/tracking": "app-1:apps/Deployment:ns-1/app-1", "deployment.kubernetes.io/revision": "4"},
        "managedFields": [
            {"manager": "kiln", "operation": "Apply", "apiVersion": "apps/v1", "fieldsType": "FieldsV1", "fieldsV1": {"f:metadata": {"f:labels": {"f:app": {}, "f:team": {}, "f:kiln.dev/app": {}}, "f:annotations": {"f:kiln.dev/tracking": {}}},
                "f:spec": {"f:replicas": {}, "f:selector": {}, "f:template": {"f:metadata": {"f:labels": {"f:app": {}}}, "f:spec": {"f:containers": {"k:{\"name\":\"app\"}": {".": {}, "f:image": {}, "f:name": {}, "f:env": {"k:{\"name\":\"MODE\"}": {".": {}, "f:name": {}, "f:value": {}}},
                    "f:ports": {"k:{\"containerPort\":8080,\"protocol\":\"TCP\"}": {".": {}, "f:containerPort": {}}}, "f:resources": {"f:requests": {"f:cpu": {}, "f:memory": {}}}}}}}}}},
            {"manager": "kube-controller-manager", "operation": "Update", "apiVersion": "apps/v1", "subresource": "status", "time": "2026-10-16T00:01:00Z", "fieldsType": "FieldsV1",
             "fieldsV1": {"f:metadata": {"f:annotations": {"f:deployment.kubernetes.io/revision": {}}}, "f:status": {"f:availableReplicas": {}, "f:conditions": {}, "f:observedGeneration": {}, "f:readyReplicas": {}, "f:replicas": {}, "f:updatedReplicas": {}}}}
        ]},
        "spec": {"replicas": 2, "progressDeadlineSeconds": 600, "revisionHistoryLimit": 10, "selector": {"matchLabels": {"app": "app-1"}},
            "strategy": {"type": "RollingUpdate", "rollingUpdate": {"maxSurge": "25%", "maxUnavailable": "25%"}},
            "template": {"metadata": {"labels": {"app": "app-1"}}, "spec": {"containers": [{"name": "app", "image": "ghcr.io/hyper-light/app:1",
                "ports": [{"containerPort": 8080, "protocol": "TCP"}], "env": [{"name": "MODE", "value": "prod"}],
                "resources": {"requests": {"cpu": "100m", "memory": "128Mi"}}, "imagePullPolicy": "IfNotPresent", "terminationMessagePath": "/dev/termination-log",
                "terminationMessagePolicy": "File"}], "restartPolicy": "Always", "terminationGracePeriodSeconds": 30, "dnsPolicy": "ClusterFirst",
                "securityContext": {}, "schedulerName": "default-scheduler"}}},
        "status": {"observedGeneration": 4, "replicas": 2, "updatedReplicas": 2, "readyReplicas": 2, "availableReplicas": 2,
            "conditions": [{"type": "Available", "status": "True", "lastUpdateTime": "2026-10-16T00:01:00Z", "lastTransitionTime": "2026-10-16T00:01:00Z", "reason": "MinimumReplicasAvailable", "message": "Deployment has minimum availability."},
                {"type": "Progressing", "status": "True", "lastUpdateTime": "2026-10-16T00:01:00Z", "lastTransitionTime": "2026-10-16T00:00:00Z", "reason": "NewReplicaSetAvailable", "message": "ReplicaSet \"app-1-5d4f\" has successfully progressed."}]}})
}

fn time<T>(name: &str, n: u32, mut f: impl FnMut() -> T) {
    let start = Instant::now();
    for _ in 0..n {
        black_box(f());
    }
    let per = start.elapsed().as_nanos() as f64 / f64::from(n);
    println!("{name:<44} {:>9.2} µs", per / 1000.0);
}

fn main() {
    let live = live_deployment();
    let text = serde_json::to_string(&live).unwrap_or_default();
    println!("object: {} bytes of JSON\n", text.len());
    let n = 20_000;
    time("parse JSON (the watch stream's cost)", n, || serde_json::from_str::<Value>(&text).ok());
    time("clone the value", n, || live.clone());
    time("health::assess", n, || health::assess(&live, &health::BuiltIn));
    time("LiveView::new (clone + split + hash)", n, || LiveView::new(live.clone(), "kiln"));
    time("hash_value (whole object)", n, || obj::hash_value(&live));
    let view = LiveView::new(live.clone(), "kiln");
    time("Blob::encode (stripped object)", n, || Blob::encode(&view.object));
    let blob = Blob::encode(&view.object);
    println!("{:<44} {:>9} bytes ({} raw)", "blob size", blob.len(), serde_json::to_vec(&view.object).map(|v| v.len()).unwrap_or(0));
    time("Blob::decode", n, || blob.decode().ok());
    let mut desired = live.clone();
    diff::normalize_desired(&mut desired);
    let owned = view.owned("kiln");
    time("diff::compare", n, || diff::compare(&desired, &view.object, owned.as_ref()));
    time("drop a value", n, || drop(live.clone()));
}
