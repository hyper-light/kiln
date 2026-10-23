//! `cloudformation`'s I/O half (docs/cd-targets.md §3): CloudFormation's Query API. The pure half,
//! `kiln_cd::providers::cloudformation`, reads its answers into objects and builds its requests.
//!
//! A stack's apply makes a change set and executes it, once CloudFormation has worked it out:
//! `CREATE` for a stack that isn't there, `UPDATE` otherwise. One that changes nothing is deleted
//! again and the stack left as it is; one that can't be made is deleted, saying why. A stack that
//! never came up (`ROLLBACK_COMPLETE`) is deleted first, as CloudFormation wants; one it's still
//! working on is left until it's done, and the next sync tries again. CloudFormation rolls back an
//! update that fails, and the stack reads Degraded until the next lands. A template too long to
//! send inline (51,200 bytes) goes through kiln's bucket.

use std::time::Duration;

use anyhow::{Result, bail};
use kiln_cd::obj::{self, ResKey};
use kiln_cd::providers::cloudformation::{self as p, GROUP};
use serde_json::{Value, json};

use super::{Cx, applied, until};
use crate::ci::integrations::aws::{self, AwsError};

const VERSION: &str = "2010-05-15";
/// How long a change set may take to be worked out.
const PLAN_LIMIT: Duration = Duration::from_secs(300);
/// How long a stack that never came up may take to go.
const GONE_LIMIT: Duration = Duration::from_secs(600);

/// An action's result, as JSON.
async fn call(cx: &Cx<'_>, action: &str, params: &[(String, String)]) -> Result<Value> {
    cx.conn.budget().await;
    let xml = cx.aws()?.query("cloudformation", action, VERSION, params).await?;
    let v = aws::xml_json(&xml);
    Ok(v.pointer(&format!("/{action}Response/{action}Result")).cloned().unwrap_or(Value::Null))
}

fn pair(k: &str, v: &str) -> (String, String) {
    (k.to_owned(), v.to_owned())
}

/// CloudFormation says a stack isn't there in a `ValidationError`.
fn missing(e: &anyhow::Error) -> bool {
    AwsError::is_not_found(e) || e.downcast_ref::<AwsError>().is_some_and(|a| a.code == "ValidationError" && a.message.contains("does not exist"))
}

/// The stack as DescribeStacks answers it.
async fn describe(cx: &Cx<'_>, name: &str) -> Result<Option<Value>> {
    match call(cx, "DescribeStacks", &[pair("StackName", name)]).await {
        Ok(v) => Ok(v.pointer("/Stacks/0").cloned()),
        Err(e) if missing(&e) => Ok(None),
        Err(e) => Err(e),
    }
}

/// The stack as an object: its template, its resources, what kiln gave it.
async fn stack(cx: &Cx<'_>, name: &str) -> Result<Option<Value>> {
    let Some(s) = describe(cx, name).await? else { return Ok(None) };
    let status = s.get("StackStatus").and_then(Value::as_str).unwrap_or_default();
    // Made by a change set that never ran: not there yet.
    if status == "REVIEW_IN_PROGRESS" {
        return Ok(None);
    }
    let template = call(cx, "GetTemplate", &[pair("StackName", name), pair("TemplateStage", "Original")]).await?.get("TemplateBody").and_then(Value::as_str).map(str::to_owned);
    let resources = call(cx, "DescribeStackResources", &[pair("StackName", name)]).await?.get("StackResources").and_then(Value::as_array).cloned().unwrap_or_default();
    let given = cx.applied(GROUP, "Stack", "", name).map(|a| a.object);
    Ok(p::live_stack(&s, template.as_deref(), &resources, given.as_ref()))
}

pub async fn list(cx: &Cx<'_>, kind: &str, names: &[(String, String)]) -> Result<Vec<Value>> {
    let mut out = Vec::new();
    if kind == "Stack" {
        for (_, name) in names {
            out.extend(stack(cx, name).await?);
        }
    }
    Ok(out)
}

/// The stacks the account holds in the region (§8.3).
pub async fn discover(cx: &Cx<'_>) -> Result<Value> {
    let stacks = call(cx, "DescribeStacks", &[]).await?.get("Stacks").and_then(Value::as_array).cloned().unwrap_or_default();
    let resources: Vec<Value> = stacks
        .iter()
        .map(|s| {
            let g = |k: &str| s.get(k).and_then(Value::as_str).unwrap_or_default();
            json!({ "kind": "Stack", "scope": "", "name": g("StackName"), "summary": format!("{} · {}", g("StackStatus"), g("LastUpdatedTime")) })
        })
        .collect();
    Ok(json!({ "scopes": [], "resources": resources }))
}

/// A stack as it runs now, as the definition that adopts it (§8.2 step 2): its template (to keep
/// in the source, its path then the definition's `template`), parameters, capabilities and tags.
pub async fn adopt(cx: &Cx<'_>, name: &str) -> Result<Value> {
    let Some(live) = stack(cx, name).await? else { bail!("no stack {name}") };
    let spec = live.get("spec").cloned().unwrap_or(Value::Null);
    let mut definition = json!({ "stack": name, "template": spec.get("template"), "parameters": spec.get("parameters") });
    if let (Some(m), Some(c)) = (definition.as_object_mut(), spec.get("capabilities")) {
        m.insert("capabilities".into(), c.clone());
    }
    if let (Some(m), Some(t)) = (definition.as_object_mut(), spec.get("tags")) {
        m.insert("tags".into(), t.clone());
    }
    Ok(json!({ "definition": definition, "note": "Keep the template in the source and name its path as `template`; a parameter CloudFormation hides reads as ****." }))
}

/// Waits until a stack that never came up is gone.
async fn delete_and_wait(cx: &Cx<'_>, name: &str) -> Result<()> {
    call(cx, "DeleteStack", &[pair("StackName", name)]).await?;
    until(cx.conn, GONE_LIMIT, &format!("deleting stack {name}"), || async move {
        Ok(match describe(cx, name).await? {
            None => Some(()),
            Some(s) if s.get("StackStatus").and_then(Value::as_str) == Some("DELETE_COMPLETE") => Some(()),
            Some(s) if s.get("StackStatus").and_then(Value::as_str) == Some("DELETE_FAILED") => bail!("stack {name} failed to delete: {}", s.get("StackStatusReason").and_then(Value::as_str).unwrap_or_default()),
            Some(_) => None,
        })
    })
    .await
}

pub async fn apply(cx: &Cx<'_>, desired: &Value) -> Result<Value> {
    match obj::kind(desired) {
        "Stack" => apply_stack(cx, desired).await,
        other => bail!("cloudformation doesn't apply {other}"),
    }
}

async fn apply_stack(cx: &Cx<'_>, desired: &Value) -> Result<Value> {
    let name = obj::name(desired);
    let mut live = describe(cx, name).await?;
    let status = live.as_ref().and_then(|s| s.get("StackStatus").and_then(Value::as_str)).unwrap_or_default().to_owned();
    match status.as_str() {
        // A stack that never came up can't be changed: it goes, and comes again.
        "ROLLBACK_COMPLETE" => {
            delete_and_wait(cx, name).await?;
            live = None;
        }
        // Left by a change set that never ran.
        "REVIEW_IN_PROGRESS" => live = None,
        s if p::busy(s) => bail!("stack {name} is {s}: kiln changes it once CloudFormation is done"),
        _ => {}
    }
    if live.is_some()
        && let Some(current) = stack(cx, name).await?
        && p::stack_matches(desired, &current)
    {
        cx.record(GROUP, "Stack", "", name, Some(applied(desired, json!({ "StackId": current.pointer("/status/stackId") })))).await?;
        return stack(cx, name).await?.ok_or_else(|| anyhow::anyhow!("stack {name} is gone"));
    }
    let kind = if live.is_some() { "UPDATE" } else { "CREATE" };
    let mut params = p::change_set_params(desired, kind);
    let template = desired.pointer("/spec/template").and_then(Value::as_str).unwrap_or_default();
    if template.len() > p::INLINE_MAX {
        let (bucket, key) = super::aws_s3::put(cx, "cloudformation", template.as_bytes(), ".template").await?;
        params.push(pair("TemplateURL", &super::aws_s3::url(cx, &bucket, &key)?));
    } else {
        params.push(pair("TemplateBody", template));
    }
    let change_set = p::change_set_name(desired);
    match call(cx, "CreateChangeSet", &params).await {
        Ok(_) => {}
        // Asked for again (a sync retried): that one.
        Err(e) if e.downcast_ref::<AwsError>().is_some_and(|a| a.code == "AlreadyExistsException" && a.message.contains("ChangeSet")) => {}
        Err(e) => return Err(e),
    }
    let ids = [pair("StackName", name), pair("ChangeSetName", &change_set)];
    let worked_out = until(cx.conn, PLAN_LIMIT, &format!("CloudFormation working out change set {change_set}"), || async {
        let cs = call(cx, "DescribeChangeSet", &ids).await?;
        Ok(match cs.get("Status").and_then(Value::as_str) {
            Some("CREATE_COMPLETE") => Some(Ok(())),
            Some("FAILED") => Some(Err(cs.get("StatusReason").and_then(Value::as_str).unwrap_or_default().to_owned())),
            _ => None,
        })
    })
    .await?;
    if let Err(reason) = worked_out {
        call(cx, "DeleteChangeSet", &ids).await?;
        if kind == "CREATE" {
            // The stack its change set made, which never ran.
            call(cx, "DeleteStack", &[pair("StackName", name)]).await?;
        }
        if !p::no_changes(&reason) {
            bail!("CloudFormation can't make stack {name}'s change set: {reason}");
        }
        cx.record(GROUP, "Stack", "", name, Some(applied(desired, json!({ "ChangeSet": Value::Null })))).await?;
        return stack(cx, name).await?.ok_or_else(|| anyhow::anyhow!("stack {name} is gone"));
    }
    call(cx, "ExecuteChangeSet", &ids).await?;
    cx.record(GROUP, "Stack", "", name, Some(applied(desired, json!({ "ChangeSet": change_set, "Type": kind })))).await?;
    // Its status says it's under way; health follows it to the end.
    stack(cx, name).await?.ok_or_else(|| anyhow::anyhow!("stack {name} is gone"))
}

pub async fn delete(cx: &Cx<'_>, key: &ResKey) -> Result<()> {
    match &*key.kind {
        "Stack" => match call(cx, "DeleteStack", &[pair("StackName", &key.name)]).await {
            Ok(_) => Ok(()),
            Err(e) if missing(&e) => Ok(()),
            Err(e) => Err(e),
        },
        other => bail!("cloudformation doesn't delete {other}"),
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use kiln_cd::providers::Target;

    use super::*;
    use crate::cd::targets::fake::{self, MemFiles, MemRecords};
    use crate::cd::targets::fake_query::{self as fq, FakeQuery, released};
    use crate::cd::targets::{Conn, Creds, key_of};
    use crate::ci::integrations::aws::{Aws, Credentials};

    const V1: &str = "AWSTemplateFormatVersion: '2010-09-09'\nParameters:\n  Env: { Type: String }\n  Secret: { Type: String, NoEcho: true }\nResources:\n  Bucket:\n    Type: AWS::S3::Bucket\n    Properties:\n      BucketName: !Sub 'shop-${Env}'\n";
    const V2: &str = "AWSTemplateFormatVersion: '2010-09-09'\nParameters:\n  Env: { Type: String }\n  Secret: { Type: String, NoEcho: true }\nResources:\n  Bucket:\n    Type: AWS::S3::Bucket\n    Properties:\n      BucketName: !Sub 'shop-${Env}'\n  Topic:\n    Type: AWS::SNS::Topic\n";
    const BROKEN: &str = "# FAIL_ON_UPDATE\nResources:\n  Bucket:\n    Type: AWS::S3::Bucket\n";

    fn spec() -> Value {
        json!({ "stack": "shop-infra", "template": "deploy/infra.yaml", "parameters": { "Env": "${{ env.name }}", "Secret": "hunter2", "Version": "${{ release.version }}" }, "capabilities": ["CAPABILITY_IAM"], "tags": { "team": "shop" } })
    }

    /// A stack's life through releases on a faked CloudFormation: the first release makes it
    /// through a CREATE change set; the same release again changes nothing (its change set fails
    /// saying so, and goes); the next adds a resource through an UPDATE; one whose update fails is
    /// rolled back by CloudFormation and reads Degraded; the release before, rolled back to, lands
    /// again; then the stack goes.
    #[test]
    fn stacks_change_through_change_sets() -> anyhow::Result<()> {
        let _one = fake::LAMBDA.lock();
        crate::rt::block_on(async {
            let fq_fake = Arc::new(FakeQuery::default());
            let base = fq::start(fq_fake.clone()).await?;
            aws::override_endpoint("cloudformation", &base);
            let target = Target::parse("cloudformation/aws-test/us-east-1").ok_or_else(|| anyhow::anyhow!("target"))?;
            let def = target.def().ok_or_else(|| anyhow::anyhow!("provider"))?;
            assert!(def.built);
            let conn = Conn::quick(target, def);
            let (records, files) = (MemRecords::default(), MemFiles::default());
            let creds = Credentials { access_key_id: "AKIDFAKE".into(), secret_access_key: "s".into(), session_token: None, expires_at: None };
            let cx = Cx { conn: &conn, creds: Creds::Aws(Aws { creds, region: "us-east-1".into() }), records: &records, files: &files };
            let names = [(String::new(), "shop-infra".to_owned())];
            let settle = || async {
                for _ in 0..10 {
                    let live = list(&cx, "Stack", &names).await?;
                    if live.first().is_some_and(|l| !p::busy(obj::str_at(l, &["status", "status"]).unwrap_or_default())) {
                        return Ok::<_, anyhow::Error>(live.into_iter().next().unwrap_or_default());
                    }
                }
                anyhow::bail!("the stack didn't settle")
            };

            // The first release: a CREATE change set, executed; the stack comes up.
            let first = released("cloudformation", spec(), "1.0.0", &[("deploy/infra.yaml", V1)])?;
            let stack1 = first.first().cloned().unwrap_or_default();
            assert_eq!(stack1.pointer("/spec/parameters/Env"), Some(&json!("staging")), "the environment's name, filled by the release");
            assert_eq!(stack1.pointer("/spec/parameters/Version"), Some(&json!("1.0.0")));
            let live = apply(&cx, &stack1).await?;
            assert_eq!(kiln_cd::providers::health(&live).map(|h| h.health), Some(kiln_cd::model::Health::Progressing), "under way: {live}");
            let up = settle().await?;
            assert_eq!(obj::str_at(&up, &["status", "status"]), Some("CREATE_COMPLETE"));
            assert!(p::stack_matches(&stack1, &up), "the hidden parameter reads as kiln gave it: {up}");
            assert_eq!(obj::label(&up, kiln_cd::engine::APP_LABEL), obj::label(&stack1, kiln_cd::engine::APP_LABEL), "tracking in the stack's tags");
            assert_eq!(up.pointer("/status/resources/0/logicalId"), Some(&json!("Bucket")));
            assert_eq!(fq_fake.state.lock().calls, ["CreateChangeSet shop-infra CREATE", "ExecuteChangeSet shop-infra"]);

            // The same again: nothing to change, nothing run.
            apply(&cx, &stack1).await?;
            assert_eq!(fq_fake.state.lock().calls.len(), 2, "a stack as kiln wants it is let be: {:?}", fq_fake.state.lock().calls);

            // The next release: an UPDATE adding a topic.
            let second = released("cloudformation", spec(), "1.1.0", &[("deploy/infra.yaml", V2)])?;
            let stack2 = second.first().cloned().unwrap_or_default();
            apply(&cx, &stack2).await?;
            let updated = settle().await?;
            assert_eq!(obj::str_at(&updated, &["status", "status"]), Some("UPDATE_COMPLETE"));
            assert!(updated.pointer("/status/resources").and_then(Value::as_array).is_some_and(|r| r.iter().any(|x| x.get("logicalId") == Some(&json!("Topic")))));
            assert_eq!(fq_fake.state.lock().calls.last().map(String::as_str), Some("ExecuteChangeSet shop-infra"));

            // A change set CloudFormation can't make is let go, saying why.
            let serverless = released("cloudformation", spec(), "1.1.1", &[("deploy/infra.yaml", "Transform: AWS::Serverless-2016-10-31\nResources:\n  Fn:\n    Type: AWS::Serverless::Function\n")])?;
            let refused = apply(&cx, serverless.first().unwrap_or(&Value::Null)).await;
            assert!(refused.is_err_and(|e| format!("{e:#}").contains("CAPABILITY_AUTO_EXPAND")));
            assert!(fq_fake.state.lock().change_sets.is_empty(), "the failed change set went");

            // One whose update fails: CloudFormation rolls it back, and the stack reads Degraded.
            let broken = released("cloudformation", spec(), "1.2.0", &[("deploy/infra.yaml", BROKEN)])?;
            apply(&cx, broken.first().unwrap_or(&Value::Null)).await?;
            // Read as the engine reads it: with what kiln last applied, the broken template.
            let rolled = cx.with_applied(settle().await?);
            assert_eq!(obj::str_at(&rolled, &["status", "status"]), Some("UPDATE_ROLLBACK_COMPLETE"));
            let h = kiln_cd::providers::health(&rolled).ok_or_else(|| anyhow::anyhow!("no health"))?;
            assert!(h.health == kiln_cd::model::Health::Degraded && h.message.contains("rolled it back") && h.message.contains("already exists"), "{h:?}");

            // Rolled back to the release before (as kiln's revert of a failed release is): what
            // CloudFormation rolled back to is that, so nothing runs, and it reads Healthy.
            let calls = fq_fake.state.lock().calls.len();
            apply(&cx, &stack2).await?;
            let back = cx.with_applied(settle().await?);
            assert!(p::stack_matches(&stack2, &back));
            assert_eq!(fq_fake.state.lock().calls.len(), calls, "no change set for what runs already");
            assert_eq!(kiln_cd::providers::health(&back).map(|h| h.health), Some(kiln_cd::model::Health::Healthy), "{back}");
            // A later release updates it as any.
            let third = released("cloudformation", spec(), "1.3.0", &[("deploy/infra.yaml", V1)])?;
            apply(&cx, third.first().unwrap_or(&Value::Null)).await?;
            assert_eq!(obj::str_at(&settle().await?, &["status", "status"]), Some("UPDATE_COMPLETE"));

            // What the account holds; then the stack goes.
            let found = discover(&cx).await?;
            assert_eq!(found.pointer("/resources/0/name"), Some(&json!("shop-infra")));
            delete(&cx, &key_of(&stack2)).await?;
            delete(&cx, &key_of(&stack2)).await?;
            let _ = settle().await;
            assert!(list(&cx, "Stack", &names).await?.is_empty(), "gone");
            Ok(())
        })
    }
}
