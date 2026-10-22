//! `aws-ecs`'s I/O half (docs/cd-targets.md §3): ECS's JSON API. The pure half,
//! `kiln_cd::providers::aws_ecs`, reads its answers into objects and builds its requests.
//!
//! A task definition's apply registers a new revision when the family's newest differs from
//! what kiln wants. A service's apply creates it, or updates it to the family's newest revision
//! and the settings kiln sets; ECS rolls the tasks out, with its circuit breaker when the
//! service asks for one, and the service's health follows the rollout.
//!
//! An app's secrets (docs/deploy-api.md §3.5) are SSM SecureString parameters kiln writes before
//! the task definition, which its containers read through `secrets`: a Parameter reads as the
//! reference kiln applied while SSM holds the version kiln wrote; another version is drift, and
//! its value never shows.

use anyhow::{Result, bail};
use kiln_cd::apps::{SECRET_CHANGED, SECRET_REF, SSM_GROUP};
use kiln_cd::obj::{self, ResKey};
use kiln_cd::providers::aws_ecs::{self as p, GROUP};
use serde_json::{Value, json};

use super::{Cx, applied};
use crate::ci::integrations::aws::AwsError;

const TARGET: &str = "AmazonEC2ContainerServiceV20141113";

async fn call(cx: &Cx<'_>, op: &str, body: &Value) -> Result<Value> {
    cx.conn.budget().await;
    cx.aws()?.json("ecs", &format!("{TARGET}.{op}"), body).await
}

async fn ssm(cx: &Cx<'_>, op: &str, body: &Value) -> Result<Value> {
    cx.conn.budget().await;
    cx.aws()?.json("ssm", &format!("AmazonSSM.{op}"), body).await
}

/// An app's secret as SSM holds it: the reference kiln applied while the parameter is at the
/// version kiln wrote, otherwise [`SECRET_CHANGED`].
fn parameter_object(cx: &Cx<'_>, p: &Value) -> Option<Value> {
    let name = p.get("Name")?.as_str()?;
    let version = p.get("Version").and_then(Value::as_i64);
    let ours = cx.applied(SSM_GROUP, "Parameter", "", name).filter(|r| version.is_some() && r.made.get("Version").and_then(Value::as_i64) == version);
    let value = ours.and_then(|r| r.object.pointer("/spec/value").cloned()).unwrap_or_else(|| Value::String(SECRET_CHANGED.into()));
    let mut o = kiln_cd::providers::object(SSM_GROUP, "Parameter", name, None, json!({ "value": value }));
    if let Value::Object(m) = &mut o {
        m.insert("status".into(), json!({ "version": version, "type": p.get("Type"), "lastModified": p.get("LastModifiedDate") }));
    }
    Some(o)
}

/// Parameters by name, as GetParameters answers (10 at a time, never their values).
async fn parameters(cx: &Cx<'_>, names: &[&str]) -> Result<Vec<Value>> {
    let mut out = Vec::new();
    for chunk in names.chunks(10) {
        let v = ssm(cx, "GetParameters", &json!({ "Names": chunk, "WithDecryption": false })).await?;
        out.extend(v.get("Parameters").and_then(Value::as_array).into_iter().flatten().filter_map(|p| parameter_object(cx, p)));
    }
    Ok(out)
}

/// ECS says a task definition or service isn't there in a `ClientException`.
fn missing(e: &anyhow::Error) -> bool {
    AwsError::is_not_found(e) || e.downcast_ref::<AwsError>().is_some_and(|a| a.code == "ClientException" && (a.message.contains("Unable to describe task definition") || a.message.contains("not found")))
}

/// The family's newest active revision, as DescribeTaskDefinition answers (with its tags).
async fn describe_family(cx: &Cx<'_>, family: &str) -> Result<Option<Value>> {
    match call(cx, "DescribeTaskDefinition", &json!({ "taskDefinition": family, "include": ["TAGS"] })).await {
        Ok(v) => {
            if let Some(arn) = v.pointer("/taskDefinition/taskDefinitionArn").and_then(Value::as_str) {
                cx.conn.memo.lock().insert(format!("ecs:newest:{family}"), arn.to_owned());
            }
            Ok(Some(v))
        }
        Err(e) if missing(&e) => Ok(None),
        Err(e) => Err(e),
    }
}

/// The family's newest active revision's ARN, as last read (reading it if it never was).
async fn newest(cx: &Cx<'_>, family: &str) -> Result<Option<String>> {
    if let Some(arn) = cx.conn.memo.lock().get(&format!("ecs:newest:{family}")).cloned() {
        return Ok(Some(arn));
    }
    Ok(describe_family(cx, family).await?.and_then(|v| v.pointer("/taskDefinition/taskDefinitionArn").and_then(Value::as_str).map(str::to_owned)))
}

/// Services of `cluster` by name, as DescribeServices answers (10 at a time); deleted ones aren't
/// there.
async fn describe_services(cx: &Cx<'_>, cluster: &str, names: &[&str]) -> Result<Vec<Value>> {
    let mut out = Vec::new();
    for chunk in names.chunks(10) {
        let v = match call(cx, "DescribeServices", &json!({ "cluster": cluster, "services": chunk, "include": ["TAGS"] })).await {
            Ok(v) => v,
            Err(e) if missing(&e) => continue,
            Err(e) => return Err(e),
        };
        out.extend(v.get("services").and_then(Value::as_array).into_iter().flatten().filter(|s| s.get("status").and_then(Value::as_str) != Some("INACTIVE")).cloned());
    }
    Ok(out)
}

async fn service(cx: &Cx<'_>, cluster: &str, name: &str) -> Result<Option<Value>> {
    let Some(svc) = describe_services(cx, cluster, &[name]).await?.into_iter().next() else { return Ok(None) };
    let family = svc.get("taskDefinition").and_then(Value::as_str).and_then(p::family_of).map(str::to_owned);
    let newest = match family {
        Some(f) => newest(cx, &f).await?,
        None => None,
    };
    Ok(p::live_service(&svc, newest.as_deref()))
}

pub async fn list(cx: &Cx<'_>, kind: &str, names: &[(String, String)]) -> Result<Vec<Value>> {
    let mut out = Vec::new();
    match kind {
        "Parameter" => out = parameters(cx, &names.iter().map(|(_, n)| n.as_str()).collect::<Vec<_>>()).await?,
        "TaskDefinition" => {
            for (_, family) in names {
                if let Some(v) = describe_family(cx, family).await? {
                    out.extend(p::live_task_definition(&v));
                }
            }
        }
        "Service" => {
            let mut by_cluster: std::collections::BTreeMap<&str, Vec<&str>> = std::collections::BTreeMap::new();
            for (ns, name) in names {
                by_cluster.entry(ns.as_str()).or_default().push(name.as_str());
            }
            for (cluster, names) in by_cluster {
                for svc in describe_services(cx, cluster, &names).await? {
                    let family = svc.get("taskDefinition").and_then(Value::as_str).and_then(p::family_of).map(str::to_owned);
                    let newest = match family {
                        Some(f) => newest(cx, &f).await?,
                        None => None,
                    };
                    out.extend(p::live_service(&svc, newest.as_deref()));
                }
            }
        }
        _ => {}
    }
    Ok(out)
}

/// Names from ARNs (`…:cluster/main` → `main`).
fn names_of(arns: Option<&Value>) -> Vec<String> {
    arns.and_then(Value::as_array).into_iter().flatten().filter_map(Value::as_str).filter_map(|a| a.rsplit('/').next()).map(str::to_owned).collect()
}

/// What the account holds in the region: its ECS clusters, and the services of `cluster` (or
/// of every cluster, up to 20) (§8.2, §8.3).
pub async fn discover(cx: &Cx<'_>, cluster: Option<&str>) -> Result<Value> {
    let clusters = names_of(call(cx, "ListClusters", &json!({ "maxResults": 100 })).await?.get("clusterArns"));
    let wanted: Vec<String> = match cluster {
        Some(c) => vec![c.to_owned()],
        None => clusters.iter().take(20).cloned().collect(),
    };
    let mut out = Vec::new();
    for c in &wanted {
        let mut names = Vec::new();
        let mut token: Option<String> = None;
        loop {
            let mut body = json!({ "cluster": c, "maxResults": 100 });
            if let (Some(t), Value::Object(m)) = (&token, &mut body) {
                m.insert("nextToken".into(), Value::String(t.clone()));
            }
            let page = call(cx, "ListServices", &body).await?;
            names.extend(names_of(page.get("serviceArns")));
            token = page.get("nextToken").and_then(Value::as_str).map(str::to_owned);
            if token.is_none() || names.len() >= 500 {
                break;
            }
        }
        let refs: Vec<&str> = names.iter().map(String::as_str).collect();
        for svc in describe_services(cx, c, &refs).await? {
            let s = |k: &str| svc.get(k).and_then(Value::as_str).unwrap_or_default();
            let n = |k: &str| svc.get(k).and_then(Value::as_i64).unwrap_or(0);
            let launch = if s("launchType").is_empty() { "capacity providers" } else { s("launchType") };
            out.push(json!({ "kind": "Service", "scope": c, "name": s("serviceName"), "summary": format!("{launch} · {} of {} running · {}", n("runningCount"), n("desiredCount"), s("taskDefinition").rsplit('/').next().unwrap_or_default()) }));
        }
    }
    Ok(json!({ "scopes": clusters, "resources": out }))
}

/// A service as it runs now, as the definition that adopts it (§8.2 step 2): its task
/// definition's revision as the task, and its settings.
pub async fn adopt(cx: &Cx<'_>, cluster: &str, name: &str) -> Result<Value> {
    if cluster.is_empty() {
        bail!("name the ECS cluster (`scope`)");
    }
    let Some(svc) = describe_services(cx, cluster, &[name]).await?.into_iter().next() else { bail!("no service {name} in {cluster}") };
    let td_arn = svc.get("taskDefinition").and_then(Value::as_str).unwrap_or_default().to_owned();
    let td = call(cx, "DescribeTaskDefinition", &json!({ "taskDefinition": td_arn })).await?;
    let mut task = p::live_task_definition(&td).and_then(|o| o.get("spec").cloned()).unwrap_or(Value::Null);
    if let Some(m) = task.as_object_mut() {
        m.remove("tags");
    }
    let mut settings = p::live_service(&svc, None).and_then(|o| o.get("spec").cloned()).unwrap_or(Value::Null);
    if let Some(m) = settings.as_object_mut() {
        m.remove("taskDefinition");
        m.remove("tags");
    }
    Ok(json!({ "definition": { "cluster": cluster, "service": name, "task": task, "service-spec": settings }, "note": Value::Null }))
}

pub async fn apply(cx: &Cx<'_>, desired: &Value) -> Result<Value> {
    match obj::kind(desired) {
        "Parameter" => apply_parameter(cx, desired).await,
        "TaskDefinition" => apply_task_definition(cx, desired).await,
        "Service" => apply_service(cx, desired).await,
        other => bail!("aws-ecs doesn't apply {other}"),
    }
}

/// Writes an app's secret, unless SSM already holds the version kiln wrote of it.
async fn apply_parameter(cx: &Cx<'_>, desired: &Value) -> Result<Value> {
    let name = obj::name(desired);
    let want = desired.pointer("/spec/value").cloned().unwrap_or(Value::Null);
    if let Some(live) = parameters(cx, &[name]).await?.into_iter().find(|l| l.pointer("/spec/value") == Some(&want)) {
        return Ok(live);
    }
    let what = want.as_str().unwrap_or_default().trim_start_matches(SECRET_REF).to_owned();
    let body = super::resolve_secrets(cx, desired, json!({ "Name": name, "Value": want, "Type": "SecureString", "Overwrite": true, "Description": format!("set by kiln: {what}") })).await?;
    let v = ssm(cx, "PutParameter", &body).await?;
    cx.record(SSM_GROUP, "Parameter", "", name, Some(applied(desired, json!({ "Version": v.get("Version") })))).await?;
    parameters(cx, &[name]).await?.into_iter().next().ok_or_else(|| anyhow::anyhow!("parameter {name} is gone"))
}

async fn apply_task_definition(cx: &Cx<'_>, desired: &Value) -> Result<Value> {
    let family = obj::name(desired);
    let live = describe_family(cx, family).await?;
    let current = live.as_ref().and_then(p::live_task_definition);
    // A secret kiln put there that the app no longer has is a new revision too.
    let arn = match current.as_ref().filter(|l| p::task_matches(desired, l) && !p::secrets_gone(desired, l)) {
        // The newest revision already is it: no new one, only kiln's tags if they're missing.
        Some(l) => {
            let arn = l.pointer("/status/taskDefinitionArn").and_then(Value::as_str).unwrap_or_default().to_owned();
            let tagged = obj::label(l, kiln_cd::engine::APP_LABEL) == obj::label(desired, kiln_cd::engine::APP_LABEL);
            if !tagged && !arn.is_empty() {
                call(cx, "TagResource", &json!({ "resourceArn": arn, "tags": p::tags_of(desired) })).await?;
            }
            arn
        }
        None => {
            let v = call(cx, "RegisterTaskDefinition", &p::register_body(desired)).await?;
            v.pointer("/taskDefinition/taskDefinitionArn").and_then(Value::as_str).ok_or_else(|| anyhow::anyhow!("ECS registered no revision"))?.to_owned()
        }
    };
    cx.conn.memo.lock().insert(format!("ecs:newest:{family}"), arn.clone());
    cx.record(GROUP, "TaskDefinition", "", family, Some(applied(desired, json!({ "taskDefinitionArn": arn })))).await?;
    let v = describe_family(cx, family).await?.ok_or_else(|| anyhow::anyhow!("task definition {family} is gone"))?;
    p::live_task_definition(&v).ok_or_else(|| anyhow::anyhow!("task definition {family} reads as nothing"))
}

async fn apply_service(cx: &Cx<'_>, desired: &Value) -> Result<Value> {
    let (cluster, name) = (obj::namespace(desired), obj::name(desired));
    if cluster.is_empty() {
        bail!("service {name} names no ECS cluster");
    }
    let td = desired.pointer("/spec/taskDefinition").and_then(Value::as_str).unwrap_or(name);
    // The family: its newest revision; an ARN: that one.
    let arn = if td.starts_with("arn:") { td.to_owned() } else { newest(cx, td).await?.ok_or_else(|| anyhow::anyhow!("task definition {td} has no active revision: apply it first"))? };
    let live = service(cx, cluster, name).await?;
    let answer = match &live {
        None => call(cx, "CreateService", &p::create_service_body(desired, &arn)).await?,
        Some(l) => {
            let fixed = p::immutable_changes(desired, l);
            if !fixed.is_empty() {
                bail!("service {name} can't change {} in place: name a new service, or delete this one first", fixed.join(", "));
            }
            let current = l.pointer("/status/taskDefinition").and_then(Value::as_str).unwrap_or_default();
            let settings_match = p::UPDATABLE.iter().all(|k| match desired.pointer(&format!("/spec/{k}")) {
                None => true,
                Some(d) => l.pointer(&format!("/spec/{k}")).is_some_and(|v| kiln_cd::providers::aws_lambda::covers(d, v)),
            });
            if current == arn && settings_match {
                json!({ "service": { "serviceArn": l.pointer("/status/serviceArn") } })
            } else {
                call(cx, "UpdateService", &p::update_service_body(desired, &arn)).await?
            }
        }
    };
    let service_arn = answer.pointer("/service/serviceArn").and_then(Value::as_str).unwrap_or_default().to_owned();
    if !service_arn.is_empty() && live.as_ref().is_some_and(|l| obj::label(l, kiln_cd::engine::APP_LABEL) != obj::label(desired, kiln_cd::engine::APP_LABEL)) {
        call(cx, "TagResource", &json!({ "resourceArn": service_arn, "tags": p::tags_of(desired) })).await?;
    }
    cx.record(GROUP, "Service", cluster, name, Some(applied(desired, json!({ "serviceArn": service_arn, "taskDefinitionArn": arn })))).await?;
    service(cx, cluster, name).await?.ok_or_else(|| anyhow::anyhow!("service {name} is gone"))
}

pub async fn delete(cx: &Cx<'_>, key: &ResKey) -> Result<()> {
    match &*key.kind {
        "Parameter" => match ssm(cx, "DeleteParameter", &json!({ "Name": &*key.name })).await {
            Ok(_) => Ok(()),
            Err(e) if e.downcast_ref::<AwsError>().is_some_and(|a| a.code == "ParameterNotFound") => Ok(()),
            Err(e) => Err(e),
        },
        "Service" => match call(cx, "DeleteService", &json!({ "cluster": &*key.namespace, "service": &*key.name, "force": true })).await {
            Ok(_) => Ok(()),
            Err(e) if missing(&e) || e.downcast_ref::<AwsError>().is_some_and(|a| a.code == "ServiceNotFoundException") => Ok(()),
            Err(e) => Err(e),
        },
        // Every active revision of the family.
        "TaskDefinition" => {
            let mut token: Option<String> = None;
            loop {
                let mut body = json!({ "familyPrefix": &*key.name, "status": "ACTIVE" });
                if let (Some(t), Value::Object(m)) = (&token, &mut body) {
                    m.insert("nextToken".into(), Value::String(t.clone()));
                }
                let page = call(cx, "ListTaskDefinitions", &body).await?;
                for arn in page.get("taskDefinitionArns").and_then(Value::as_array).into_iter().flatten().filter_map(Value::as_str) {
                    // familyPrefix matches by prefix: only this family's own.
                    if p::family_of(arn) == Some(&*key.name) {
                        call(cx, "DeregisterTaskDefinition", &json!({ "taskDefinition": arn })).await?;
                    }
                }
                token = page.get("nextToken").and_then(Value::as_str).map(str::to_owned);
                if token.is_none() {
                    break;
                }
            }
            cx.conn.memo.lock().remove(&format!("ecs:newest:{}", &*key.name));
            Ok(())
        }
        other => bail!("aws-ecs doesn't delete {other}"),
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use kiln_cd::providers::{RenderInput, Target};
    use serde_json::Map;

    use super::*;
    use crate::cd::targets::fake::{self, Fake, MemRecords};
    use crate::cd::targets::{Conn, Creds, Cx, key_of};
    use crate::ci::integrations::aws::{self, Aws, Credentials};

    fn desired(image: &str, count: u32) -> anyhow::Result<Vec<Value>> {
        let spec = json!({ "cluster": "main", "service": "api", "task": { "family": "api", "cpu": 256, "memory": 512, "containerDefinitions": [{ "name": "api", "image": image, "portMappings": [{ "containerPort": 8080 }] }] }, "set": { "service.desiredCount": count, "service.launchType": "FARGATE" } });
        let none = Map::new();
        let objs = p::render(&RenderInput { spec: &spec, target: &none, app: "api", namespace: "main", revision: "abc", read: &|_| None }).map_err(|e| anyhow::anyhow!("{e}"))?;
        Ok(objs.into_iter().map(|o| fake::tracked(o, "default/api-production")).collect())
    }

    /// A service's life on a faked ECS: a revision registered and a service running it; the
    /// same definition again registers nothing; a new image registers a revision and the
    /// service moves to it; a service moved by hand reads as drift and moves back; then gone.
    #[test]
    fn services_run_the_newest_revision() -> anyhow::Result<()> {
        let _one = fake::LAMBDA.lock();
        crate::rt::block_on(async {
            let fake = Arc::new(Fake::default());
            let base = fake::start(fake.clone()).await?;
            aws::override_endpoint("ecs", &base);
            let target = Target::parse("aws-ecs/aws-test/us-east-1").ok_or_else(|| anyhow::anyhow!("target"))?;
            let def = target.def().ok_or_else(|| anyhow::anyhow!("provider"))?;
            let conn = Conn { settle_every: std::time::Duration::from_millis(5), gap: std::time::Duration::ZERO, ..Conn::new(target, def) };
            let records = MemRecords::default();
            let creds = Credentials { access_key_id: "AKIDFAKE".into(), secret_access_key: "s".into(), session_token: None, expires_at: None };
            let files = fake::MemFiles::default();
            let cx = Cx { conn: &conn, creds: Creds::Aws(Aws { creds, region: "us-east-1".into() }), records: &records, files: &files };

            let objs = desired("acme/api@sha256:aaa", 2)?;
            let (td, svc) = (objs.first().cloned().unwrap_or_default(), objs.get(1).cloned().unwrap_or_default());
            let live_td = apply(&cx, &td).await?;
            assert!(p::task_matches(&td, &live_td));
            let live = apply(&cx, &svc).await?;
            assert_eq!(live.pointer("/spec/taskDefinition"), Some(&json!("api")));
            assert_eq!(obj::label(&live, kiln_cd::engine::APP_LABEL), obj::label(&svc, kiln_cd::engine::APP_LABEL), "tracking in the service's tags");
            apply(&cx, &td).await?;
            apply(&cx, &svc).await?;
            assert_eq!(fake.state.lock().calls, ["RegisterTaskDefinition api:1", "CreateService main/api api:1"]);

            let objs = desired("acme/api@sha256:bbb", 2)?;
            let (td2, svc2) = (objs.first().cloned().unwrap_or_default(), objs.get(1).cloned().unwrap_or_default());
            apply(&cx, &td2).await?;
            let names = [("main".to_owned(), "api".to_owned())];
            let behind = list(&cx, "Service", &names).await?;
            assert_eq!(behind.first().and_then(|o| o.pointer("/spec/taskDefinition")), Some(&json!("arn:aws:ecs:us-east-1:1:task-definition/api:1")), "on an older revision until it moves");
            apply(&cx, &svc2).await?;
            let moved = list(&cx, "Service", &names).await?;
            assert_eq!(moved.first().and_then(|o| o.pointer("/spec/taskDefinition")), Some(&json!("api")));

            // A person rolls the service back by hand: drift, and kiln's apply moves it forward.
            call(&cx, "UpdateService", &json!({ "cluster": "main", "service": "api", "taskDefinition": "arn:aws:ecs:us-east-1:1:task-definition/api:1" })).await?;
            let drifted = list(&cx, "Service", &names).await?;
            assert_ne!(drifted.first().and_then(|o| o.pointer("/spec/taskDefinition")), Some(&json!("api")));
            apply(&cx, &svc2).await?;
            assert_eq!(fake.state.lock().calls.last().map(String::as_str), Some("UpdateService main/api api:2"));

            // The launch type can't change in place.
            let mut ec2 = svc2.clone();
            kiln_cd::providers::set_path(&mut ec2, "spec.launchType", json!("EC2")).map_err(|e| anyhow::anyhow!("{e}"))?;
            assert!(apply(&cx, &ec2).await.is_err_and(|e| format!("{e:#}").contains("launchType")));

            // What the account holds, and a service adopted as it runs.
            let found = discover(&cx, None).await?;
            assert_eq!(found.pointer("/scopes/0"), Some(&json!("main")));
            assert_eq!(found.pointer("/resources/0/name"), Some(&json!("api")));
            let adopted = adopt(&cx, "main", "api").await?;
            assert_eq!(adopted.pointer("/definition/task/containerDefinitions/0/image"), Some(&json!("acme/api@sha256:bbb")));
            assert_eq!(adopted.pointer("/definition/service-spec/desiredCount"), Some(&json!(2)));
            assert_eq!(adopted.pointer("/definition/task/revision"), None);

            delete(&cx, &key_of(&svc2)).await?;
            delete(&cx, &key_of(&td2)).await?;
            assert!(list(&cx, "Service", &names).await?.is_empty());
            assert!(list(&cx, "TaskDefinition", &[(String::new(), "api".to_owned())]).await?.is_empty());
            Ok(())
        })
    }

    /// An app's secrets on ECS (docs/deploy-api.md §3.5): a SecureString parameter each, whose
    /// value goes to SSM alone, read by the task's containers through `secrets`; the parameter
    /// reads as kiln's reference while SSM holds the version kiln wrote. A value changed by hand
    /// is drift, never shown; a new version writes the parameter and registers a revision.
    #[test]
    fn secrets_reach_the_tasks_through_ssm() -> anyhow::Result<()> {
        let _one = fake::LAMBDA.lock();
        crate::rt::block_on(async {
            let fake = Arc::new(Fake::default());
            let base = fake::start(fake.clone()).await?;
            for service in ["ecs", "ssm"] {
                aws::override_endpoint(service, &base);
            }
            let target = Target::parse("aws-ecs/aws-test/us-east-1").ok_or_else(|| anyhow::anyhow!("target"))?;
            let def = target.def().ok_or_else(|| anyhow::anyhow!("provider"))?;
            let conn = Conn { settle_every: std::time::Duration::from_millis(5), gap: std::time::Duration::ZERO, ..Conn::new(target, def) };
            let records = MemRecords::default();
            let creds = Credentials { access_key_id: "AKIDFAKE".into(), secret_access_key: "s".into(), session_token: None, expires_at: None };
            let files = fake::MemFiles::default();
            let cx = Cx { conn: &conn, creds: Creds::Aws(Aws { creds, region: "us-east-1".into() }), records: &records, files: &files };
            files.1.lock().insert("kiln-secret:app/api/production/DB_PASSWORD@1".into(), b"hunter2".to_vec());
            files.1.lock().insert("kiln-secret:app/api/production/DB_PASSWORD@2".into(), b"hunter3".to_vec());
            let with = |version: u32| -> anyhow::Result<Vec<Value>> {
                let mut objs = desired("acme/api@sha256:aaa", 1)?;
                kiln_cd::apps::platform_secrets(&mut objs, "app/api/production", &[("DB_PASSWORD".into(), version)]).map_err(|e| anyhow::anyhow!("{e}"))?;
                Ok(objs.into_iter().map(|o| fake::tracked(o, "default/api-production")).collect())
            };
            let objs = with(1)?;
            let param = objs.iter().find(|o| obj::kind(o) == "Parameter").cloned().unwrap_or_default();
            let td = objs.iter().find(|o| obj::kind(o) == "TaskDefinition").cloned().unwrap_or_default();
            assert_eq!(kiln_cd::sync::wave_of(&param), -1, "before the task definition");
            let name = "/kiln/app/api/production/DB_PASSWORD";

            let live = cx.with_applied(apply(&cx, &param).await?);
            assert_eq!(fake.state.lock().parameters.get(name).map(|(v, _)| v.clone()).as_deref(), Some("hunter2"), "the value is SSM's");
            assert_eq!(live.pointer("/spec/value"), Some(&json!("kiln-secret:app/api/production/DB_PASSWORD@1")));
            assert!(!live.to_string().contains("hunter2") && !format!("{:?}", records.0.lock()).contains("hunter2"), "no value in what kiln shows or keeps");
            let live_td = apply(&cx, &td).await?;
            assert_eq!(live_td.pointer("/spec/containerDefinitions/0/secrets"), Some(&json!([{ "name": "DB_PASSWORD", "valueFrom": name }])));
            assert!(p::task_matches(&td, &live_td));
            apply(&cx, &param).await?;
            assert_eq!(fake.state.lock().calls, [format!("PutParameter {name}"), "RegisterTaskDefinition api:1".to_owned()], "the same secret again changes nothing");

            // Changed by hand: drift, the value unshown; kiln's apply puts its own back.
            if let Some(p) = fake.state.lock().parameters.get_mut(name) {
                *p = ("handmade".into(), p.1 + 1);
            }
            let drifted = list(&cx, "Parameter", &[(String::new(), name.to_owned())]).await?.into_iter().next().unwrap_or_default();
            assert_eq!(drifted.pointer("/spec/value"), Some(&json!(kiln_cd::apps::SECRET_CHANGED)));
            apply(&cx, &param).await?;
            assert_eq!(fake.state.lock().parameters.get(name).map(|(v, _)| v.clone()).as_deref(), Some("hunter2"));

            // A new version: the parameter, then (by wave) a revision whose label rolls the tasks.
            let mut objs: Vec<Value> = with(2)?.into_iter().filter(|o| matches!(obj::kind(o), "Parameter" | "TaskDefinition")).collect();
            objs.sort_by_key(kiln_cd::sync::wave_of);
            for o in &objs {
                apply(&cx, o).await?;
            }
            assert_eq!(fake.state.lock().parameters.get(name).map(|(v, _)| v.clone()).as_deref(), Some("hunter3"));
            assert_eq!(fake.state.lock().calls.last().map(String::as_str), Some("RegisterTaskDefinition api:2"));

            // Taken out of the app: gone from SSM.
            delete(&cx, &key_of(&param)).await?;
            delete(&cx, &key_of(&param)).await?;
            assert!(list(&cx, "Parameter", &[(String::new(), name.to_owned())]).await?.is_empty());
            Ok(())
        })
    }
}
