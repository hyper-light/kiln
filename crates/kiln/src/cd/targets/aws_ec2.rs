//! `aws-ec2`'s I/O half (docs/cd-targets.md §3): EC2's and Auto Scaling's Query APIs. The pure
//! half, `kiln_cd::providers::aws_ec2`, reads their answers into objects and builds their requests.
//!
//! A launch template's apply makes it, or a version of it, unless a version kiln made already
//! holds what kiln wants (a rollback's is used again, not copied); that version becomes the
//! template's default. Kiln keeps each version it made by the hash of its data. A group's apply
//! makes it on that version, or moves it there by an instance refresh, its preferences the
//! definition's: with `AutoRollback`, a refresh that fails puts back what ran, and the group reads
//! Degraded until kiln asks for what runs. A refresh under way onto another version is left to
//! finish, and the next sync tries again.

use anyhow::{Result, bail};
use kiln_cd::obj::{self, ResKey};
use kiln_cd::providers::aws_ec2::{self as p, ASG, EC2};
use serde_json::{Map, Value, json};

use super::{Cx, applied};
use crate::ci::integrations::aws::{self, AwsError};

const EC2_VERSION: &str = "2016-11-15";
const ASG_VERSION: &str = "2011-01-01";

/// An EC2 action's answer, as JSON (EC2's answers have no `Result` element).
async fn ec2(cx: &Cx<'_>, action: &str, params: &[(String, String)]) -> Result<Value> {
    cx.conn.budget().await;
    let xml = cx.aws()?.query("ec2", action, EC2_VERSION, params).await?;
    Ok(aws::xml_json(&xml).pointer(&format!("/{action}Response")).cloned().unwrap_or(Value::Null))
}

/// An Auto Scaling action's result, as JSON.
async fn asg(cx: &Cx<'_>, action: &str, params: &[(String, String)]) -> Result<Value> {
    cx.conn.budget().await;
    let xml = cx.aws()?.query("autoscaling", action, ASG_VERSION, params).await?;
    Ok(aws::xml_json(&xml).pointer(&format!("/{action}Response/{action}Result")).cloned().unwrap_or(Value::Null))
}

fn pair(k: &str, v: &str) -> (String, String) {
    (k.to_owned(), v.to_owned())
}

fn missing(e: &anyhow::Error) -> bool {
    AwsError::is_not_found(e) || e.downcast_ref::<AwsError>().is_some_and(|a| a.code.contains("NotFound") || (a.code == "ValidationError" && a.message.contains("not found")))
}

async fn template(cx: &Cx<'_>, name: &str) -> Result<Option<Value>> {
    match ec2(cx, "DescribeLaunchTemplates", &[pair("LaunchTemplateName.1", name)]).await {
        Ok(v) => Ok(v.pointer("/launchTemplates/0").cloned()),
        Err(e) if missing(&e) => Ok(None),
        Err(e) => Err(e),
    }
}

/// The version kiln made a template's data at, and the versions it made, by their data's hash.
fn made(cx: &Cx<'_>, name: &str) -> (Option<String>, Map<String, Value>) {
    let record = cx.applied(EC2, "LaunchTemplate", "", name);
    let version = record.as_ref().and_then(|r| r.made.get("version").and_then(Value::as_str).map(str::to_owned));
    let versions = record.and_then(|r| r.made.get("versions").and_then(Value::as_object).cloned()).unwrap_or_default();
    (version, versions)
}

async fn template_object(cx: &Cx<'_>, name: &str) -> Result<Option<Value>> {
    let Some(t) = template(cx, name).await? else { return Ok(None) };
    let record = cx.applied(EC2, "LaunchTemplate", "", name);
    let (version, _) = made(cx, name);
    Ok(p::live_template(&t, record.as_ref().zip(version.as_deref()).map(|(r, v)| (&r.object, v))))
}

/// The group as DescribeAutoScalingGroups answers it, with its newest instance refresh.
async fn group(cx: &Cx<'_>, name: &str) -> Result<Option<(Value, Option<Value>)>> {
    let v = asg(cx, "DescribeAutoScalingGroups", &[pair("AutoScalingGroupNames.member.1", name)]).await?;
    let Some(g) = v.pointer("/AutoScalingGroups/0").cloned() else { return Ok(None) };
    let r = asg(cx, "DescribeInstanceRefreshes", &[pair("AutoScalingGroupName", name), pair("MaxRecords", "1")]).await?;
    Ok(Some((g, r.pointer("/InstanceRefreshes/0").cloned())))
}

async fn group_object(cx: &Cx<'_>, name: &str) -> Result<Option<Value>> {
    let Some((g, refresh)) = group(cx, name).await? else { return Ok(None) };
    let lt = g.pointer("/LaunchTemplate/LaunchTemplateName").and_then(Value::as_str).unwrap_or_default().to_owned();
    let (kilns, _) = made(cx, &lt);
    Ok(p::live_group(&g, refresh.as_ref(), kilns.as_deref()))
}

pub async fn list(cx: &Cx<'_>, kind: &str, names: &[(String, String)]) -> Result<Vec<Value>> {
    let mut out = Vec::new();
    for (_, name) in names {
        let o = match kind {
            "LaunchTemplate" => template_object(cx, name).await?,
            "AutoScalingGroup" => group_object(cx, name).await?,
            _ => None,
        };
        out.extend(o);
    }
    Ok(out)
}

/// The account's Auto Scaling groups in the region (§8.3).
pub async fn discover(cx: &Cx<'_>) -> Result<Value> {
    let groups = asg(cx, "DescribeAutoScalingGroups", &[]).await?.get("AutoScalingGroups").and_then(Value::as_array).cloned().unwrap_or_default();
    let resources: Vec<Value> = groups
        .iter()
        .map(|g| {
            let s = |k: &str| g.get(k).and_then(Value::as_str).unwrap_or_default();
            let n = g.get("Instances").and_then(Value::as_array).map_or(0, Vec::len);
            json!({ "kind": "AutoScalingGroup", "scope": "", "name": s("AutoScalingGroupName"), "summary": format!("{} · version {} · {n} of {} instances", g.pointer("/LaunchTemplate/LaunchTemplateName").and_then(Value::as_str).unwrap_or("no launch template"), g.pointer("/LaunchTemplate/Version").and_then(Value::as_str).unwrap_or("?"), s("DesiredCapacity")) })
        })
        .collect();
    Ok(json!({ "scopes": [], "resources": resources }))
}

/// A group as it runs now, as the definition that adopts it (§8.2 step 2): its launch template and
/// sizes; the image is CI's to build.
pub async fn adopt(cx: &Cx<'_>, name: &str) -> Result<Value> {
    let Some((g, _)) = group(cx, name).await? else { bail!("no Auto Scaling group {name}") };
    let s = |k: &str| g.get(k).and_then(Value::as_str).unwrap_or_default().to_owned();
    let lt = g.pointer("/LaunchTemplate/LaunchTemplateName").and_then(Value::as_str).unwrap_or_default().to_owned();
    Ok(json!({ "definition": { "group": name, "launch-template": lt, "image": "${{ artifacts.ami }}", "group-spec": { "MinSize": s("MinSize"), "MaxSize": s("MaxSize"), "DesiredCapacity": s("DesiredCapacity"), "VPCZoneIdentifier": s("VPCZoneIdentifier") } }, "note": "Its template's data is the template's own as it is; set what kiln should own in `template`." }))
}

pub async fn apply(cx: &Cx<'_>, desired: &Value) -> Result<Value> {
    match obj::kind(desired) {
        "LaunchTemplate" => apply_template(cx, desired).await,
        "AutoScalingGroup" => apply_group(cx, desired).await,
        other => bail!("aws-ec2 doesn't apply {other}"),
    }
}

async fn apply_template(cx: &Cx<'_>, desired: &Value) -> Result<Value> {
    let name = obj::name(desired);
    let hash = p::data_hash(desired);
    let revision = obj::annotation(desired, kiln_cd::engine::TRACKING).unwrap_or_default().chars().take(40).collect::<String>();
    let (_, mut versions) = made(cx, name);
    let version = match template(cx, name).await? {
        None => {
            let v = ec2(cx, "CreateLaunchTemplate", &p::template_params(desired, true, &revision)).await?;
            versions.clear();
            v.pointer("/launchTemplate/latestVersionNumber").and_then(Value::as_str).unwrap_or("1").to_owned()
        }
        Some(t) => {
            let default = t.get("defaultVersionNumber").and_then(Value::as_str).unwrap_or_default().to_owned();
            let version = match versions.get(&hash).and_then(Value::as_str) {
                // A version kiln made of this very data (a rollback's): used again.
                Some(v) => v.to_owned(),
                None => {
                    let v = ec2(cx, "CreateLaunchTemplateVersion", &p::template_params(desired, false, &revision)).await?;
                    v.pointer("/launchTemplateVersion/versionNumber").and_then(Value::as_str).ok_or_else(|| anyhow::anyhow!("EC2 made no version of {name}"))?.to_owned()
                }
            };
            if version != default {
                ec2(cx, "ModifyLaunchTemplate", &[pair("LaunchTemplateName", name), pair("SetDefaultVersion", &version)]).await?;
            }
            version
        }
    };
    versions.insert(hash.clone(), Value::String(version.clone()));
    cx.record(EC2, "LaunchTemplate", "", name, Some(applied(desired, json!({ "version": version, "hash": hash, "versions": versions })))).await?;
    template_object(cx, name).await?.ok_or_else(|| anyhow::anyhow!("launch template {name} is gone"))
}

async fn apply_group(cx: &Cx<'_>, desired: &Value) -> Result<Value> {
    let name = obj::name(desired);
    let lt = obj::str_at(desired, &["spec", "launchTemplate", "name"]).unwrap_or_default().to_owned();
    let (kilns, _) = made(cx, &lt);
    let Some(version) = kilns else { bail!("launch template {lt} has no version kiln made: apply it first") };
    match group(cx, name).await? {
        None => {
            asg(cx, "CreateAutoScalingGroup", &p::create_group_params(desired, &version)).await?;
        }
        Some((g, refresh)) => {
            let live = p::live_group(&g, refresh.as_ref(), Some(&version)).ok_or_else(|| anyhow::anyhow!("group {name} reads as nothing"))?;
            if !p::settings_match(desired, &live) {
                asg(cx, "UpdateAutoScalingGroup", &p::update_group_params(desired)).await?;
            }
            let running = g.pointer("/LaunchTemplate/Version").and_then(Value::as_str).unwrap_or_default();
            let under_way = refresh.as_ref().filter(|r| matches!(r.get("Status").and_then(Value::as_str), Some("Pending" | "InProgress" | "Baking" | "Cancelling" | "RollbackInProgress")));
            match under_way {
                // Onto what kiln wants already: it's moving there.
                Some(r) if r.pointer("/DesiredConfiguration/LaunchTemplate/Version").and_then(Value::as_str) == Some(version.as_str()) => {}
                Some(r) => bail!("group {name} is refreshing onto version {}: kiln moves it once that's done", r.pointer("/DesiredConfiguration/LaunchTemplate/Version").and_then(Value::as_str).unwrap_or("?")),
                None if running != version => {
                    asg(cx, "StartInstanceRefresh", &p::refresh_params(desired, &version)).await?;
                }
                None => {}
            }
        }
    }
    cx.record(ASG, "AutoScalingGroup", "", name, Some(applied(desired, json!({ "version": version })))).await?;
    group_object(cx, name).await?.ok_or_else(|| anyhow::anyhow!("group {name} is gone"))
}

pub async fn delete(cx: &Cx<'_>, key: &ResKey) -> Result<()> {
    let r = match &*key.kind {
        "AutoScalingGroup" => asg(cx, "DeleteAutoScalingGroup", &[pair("AutoScalingGroupName", &key.name), pair("ForceDelete", "true")]).await.map(|_| ()),
        "LaunchTemplate" => ec2(cx, "DeleteLaunchTemplate", &[pair("LaunchTemplateName", &key.name)]).await.map(|_| ()),
        other => bail!("aws-ec2 doesn't delete {other}"),
    };
    match r {
        Err(e) if missing(&e) => Ok(()),
        other => other,
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use kiln_cd::model::Health;
    use kiln_cd::providers::Target;

    use super::*;
    use crate::cd::targets::fake::{self, MemFiles, MemRecords};
    use crate::cd::targets::fake_query::{self as fq, FakeQuery, released};
    use crate::cd::targets::{Conn, Creds, key_of};
    use crate::ci::integrations::aws::{Aws, Credentials};

    fn spec() -> Value {
        json!({ "group": "shop-api", "launch-template": "shop-api", "image": "${{ release.artifacts.ami.tag }}", "template": { "InstanceType": "t3.small", "SecurityGroupIds": ["sg-1"] }, "group-spec": { "MinSize": 2, "MaxSize": 4, "DesiredCapacity": 2, "VPCZoneIdentifier": "subnet-1,subnet-2" }, "refresh": { "MinHealthyPercentage": 90, "AutoRollback": true } })
    }

    /// A group through releases on a faked EC2 and Auto Scaling: the first makes the template and
    /// the group on it; the same again makes nothing; the next makes a version, the template's
    /// default, and refreshes the group onto it; one whose instances fail their health checks is
    /// rolled back by Auto Scaling and reads Degraded; the release before, rolled back to, uses
    /// the version kiln made of it again and starts no refresh, the group Healthy on it; then
    /// they go.
    #[test]
    fn groups_refresh_onto_each_releases_template() -> anyhow::Result<()> {
        let _one = fake::LAMBDA.lock();
        crate::rt::block_on(async {
            let q = Arc::new(FakeQuery::default());
            let base = fq::start(q.clone()).await?;
            for service in ["ec2", "autoscaling"] {
                aws::override_endpoint(service, &base);
            }
            let target = Target::parse("aws-ec2/aws-test/us-east-1").ok_or_else(|| anyhow::anyhow!("target"))?;
            let def = target.def().ok_or_else(|| anyhow::anyhow!("provider"))?;
            assert!(def.built);
            let conn = Conn::quick(target, def);
            let (records, files) = (MemRecords::default(), MemFiles::default());
            let creds = Credentials { access_key_id: "AKIDFAKE".into(), secret_access_key: "s".into(), session_token: None, expires_at: None };
            let cx = Cx { conn: &conn, creds: Creds::Aws(Aws { creds, region: "us-east-1".into() }), records: &records, files: &files };
            let names = [(String::new(), "shop-api".to_owned())];
            let cxr = &cx;
            let release = |version: &str, ami: &str| {
                let made = released("aws-ec2", spec(), version, &[], &[("ami", ami)]);
                async move {
                    let objs = made?;
                    for o in &objs {
                        apply(cxr, o).await?;
                    }
                    Ok::<_, anyhow::Error>(objs)
                }
            };
            let settled = || async {
                for _ in 0..12 {
                    let live = list(&cx, "AutoScalingGroup", &names).await?.into_iter().next().unwrap_or_default();
                    let busy = live.pointer("/status/refresh/status").and_then(Value::as_str).is_some_and(|s| matches!(s, "Pending" | "InProgress" | "RollbackInProgress"));
                    if !busy {
                        return Ok::<_, anyhow::Error>(live);
                    }
                }
                anyhow::bail!("the group didn't settle")
            };
            let h = |o: &Value| kiln_cd::providers::health(o).map(|h| h.health);

            // The first release: the template, and the group on it.
            let first = release("1.0.0", "ami-0aaa").await?;
            let up = settled().await?;
            assert_eq!((up.pointer("/spec/launchTemplate/version"), h(&up)), (Some(&json!(p::KILNS)), Some(Health::Healthy)), "{up}");
            assert!(p::settings_match(first.get(1).unwrap_or(&Value::Null), &up));
            assert_eq!(q.state.lock().calls, ["CreateLaunchTemplate shop-api", "CreateAutoScalingGroup shop-api 1"]);
            let lt = list(&cx, "LaunchTemplate", &names).await?.into_iter().next().unwrap_or_default();
            assert_eq!(lt.get("spec"), first.first().and_then(|o| o.get("spec")), "the template reads as kiln made it");

            // The same again: nothing.
            release("1.0.0", "ami-0aaa").await?;
            assert_eq!(q.state.lock().calls.len(), 2, "{:?}", q.state.lock().calls);

            // The next: a version, the default, and a refresh onto it.
            release("1.1.0", "ami-0bbb").await?;
            let moving = list(&cx, "AutoScalingGroup", &names).await?.into_iter().next().unwrap_or_default();
            assert_eq!(h(&moving), Some(Health::Progressing), "{moving}");
            let up2 = settled().await?;
            assert_eq!((up2.pointer("/status/version"), h(&up2)), (Some(&json!("2")), Some(Health::Healthy)));
            assert!(q.state.lock().calls.iter().any(|c| c == "ModifyLaunchTemplate shop-api default 2") && q.state.lock().calls.iter().any(|c| c == "StartInstanceRefresh shop-api 2"));

            // One whose instances fail: Auto Scaling rolls it back, and the group reads Degraded.
            release("1.2.0", "ami-0bad").await?;
            let failed = settled().await?;
            assert_eq!(failed.pointer("/status/refresh/status"), Some(&json!("RollbackSuccessful")));
            assert_eq!((failed.pointer("/status/version"), h(&failed)), (Some(&json!("2")), Some(Health::Degraded)), "{failed}");

            // Rolled back to the release before: its version again, no copy, no refresh.
            let calls = q.state.lock().calls.len();
            release("1.1.0", "ami-0bbb").await?;
            let back = settled().await?;
            assert_eq!((back.pointer("/spec/launchTemplate/version"), h(&back)), (Some(&json!(p::KILNS)), Some(Health::Healthy)), "{back}");
            let new_calls: Vec<String> = q.state.lock().calls.get(calls..).unwrap_or_default().to_vec();
            assert_eq!(new_calls, ["ModifyLaunchTemplate shop-api default 2"], "the version kiln made before, made the default again");

            // A version made the default by someone else is drift.
            ec2(&cx, "ModifyLaunchTemplate", &[pair("LaunchTemplateName", "shop-api"), pair("SetDefaultVersion", "1")]).await?;
            let drifted = list(&cx, "LaunchTemplate", &names).await?.into_iter().next().unwrap_or_default();
            assert_eq!(drifted.pointer("/spec/data/madeBy"), Some(&json!("someone else")));

            // What the account holds; then they go.
            let found = discover(&cx).await?;
            assert_eq!(found.pointer("/resources/0/name"), Some(&json!("shop-api")));
            assert_eq!(adopt(&cx, "shop-api").await?.pointer("/definition/launch-template"), Some(&json!("shop-api")));
            delete(&cx, &key_of(first.get(1).unwrap_or(&Value::Null))).await?;
            delete(&cx, &key_of(first.first().unwrap_or(&Value::Null))).await?;
            delete(&cx, &key_of(first.first().unwrap_or(&Value::Null))).await?;
            assert!(list(&cx, "AutoScalingGroup", &names).await?.is_empty() && list(&cx, "LaunchTemplate", &names).await?.is_empty());
            Ok(())
        })
    }
}
