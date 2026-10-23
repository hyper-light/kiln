//! `aws-ec2` (docs/cd-targets.md §3): a launch template's versions, and the Auto Scaling group
//! that runs one, moved by an instance refresh.
//!
//! ```yaml
//! api:
//!   aws-ec2:
//!     group: shop-api
//!     launch-template: shop-api
//!     image: "${{ artifacts.ami }}"          # an AMI (ami-…)
//!     template: deploy/launch.json            # LaunchTemplateData: InstanceType, UserData, …
//!     group-spec: { MinSize: 2, MaxSize: 6, VPCZoneIdentifier: "subnet-1,subnet-2" }  # for a group kiln makes
//!     refresh: { MinHealthyPercentage: 90, InstanceWarmup: 60, CheckpointPercentages: [50], AutoRollback: true }
//! ```
//!
//! - **LaunchTemplate** (wave 0): `spec.data` is its LaunchTemplateData, the image in it. An apply
//!   makes the template, or a new version of it when no version kiln made holds what kiln wants
//!   (one that does, a rollback's, is used again); that version is the template's default. Another
//!   version made the default by anyone else reads as drift.
//! - **AutoScalingGroup** (wave 1): runs the launch template at the version kiln made
//!   (`spec.launchTemplate.version: kiln`). A group on another version is moved by an instance
//!   refresh (its `refresh` preferences, `Rolling`, the group's template set as it completes): with
//!   `AutoRollback`, one that fails puts back what ran. A group kiln makes is made on it.
//! - **Instance** (read only): the group's, in its status.

use serde_json::{Map, Value, json};

use super::{RenderInput, apply_sets, file_or_object, file_settings, object, status, text};
use crate::health::Status;
use crate::model::Health;

pub const EC2: &str = "ec2.aws.kiln.dev";
pub const ASG: &str = "autoscaling.aws.kiln.dev";

/// What a group reads as when it runs the version kiln made.
pub const KILNS: &str = "kiln";

/// The files a render reads.
pub fn files(spec: &Value) -> Vec<String> {
    file_settings(spec, &["template", "group-spec"])
}

fn wave(mut o: Value, w: i64) -> Value {
    if let Some(ann) = crate::obj::ensure_map(&mut o, &["metadata", "annotations"]) {
        ann.insert(crate::sync::KILN_WAVE.into(), Value::String(w.to_string()));
    }
    o
}

/// The settings of a group a refresh's preferences and a group's spec may carry.
const GROUP_SETTINGS: &[&str] = &["MinSize", "MaxSize", "DesiredCapacity", "VPCZoneIdentifier", "TargetGroupARNs", "HealthCheckType", "HealthCheckGracePeriod", "DefaultInstanceWarmup"];

/// An app's objects.
pub fn render(input: &RenderInput<'_>) -> Result<Vec<Value>, String> {
    let group = text(input.spec, "group").ok_or("aws-ec2 needs group: the Auto Scaling group")?;
    let template = text(input.spec, "launch-template").ok_or("aws-ec2 needs launch-template: the launch template")?;
    let mut data = match file_or_object(input, "template")? {
        Some(Value::Object(m)) => Value::Object(m),
        Some(_) => return Err("template: an object (LaunchTemplateData)".into()),
        None => Value::Object(Map::new()),
    };
    // A file copied from describe-launch-template-versions holds the data under its own key.
    if let Value::Object(m) = &mut data
        && let Some(Value::Object(inner)) = m.remove("LaunchTemplateData")
    {
        *m = inner;
    }
    if let Some(image) = text(input.spec, "image") {
        if !image.starts_with("ami-") {
            return Err(format!("image `{image}`: an AMI's id (ami-…)"));
        }
        if let Value::Object(m) = &mut data {
            m.insert("ImageId".into(), Value::String(image.to_owned()));
        }
    }
    apply_sets(&mut data, input.spec.get("set"))?;
    if data.get("ImageId").and_then(Value::as_str).is_none_or(str::is_empty) {
        return Err("aws-ec2 needs an image: `image` (an AMI, ${{ artifacts.* }}), or ImageId in the template".into());
    }
    let mut spec = match file_or_object(input, "group-spec")? {
        Some(Value::Object(m)) => m,
        Some(_) => return Err("group-spec: an object (CreateAutoScalingGroup's input)".into()),
        None => Map::new(),
    };
    spec.retain(|k, _| GROUP_SETTINGS.contains(&k.as_str()));
    spec.insert("launchTemplate".into(), json!({ "name": template, "version": KILNS }));
    match input.spec.get("refresh") {
        None | Some(Value::Null) => {}
        Some(Value::Object(r)) => {
            spec.insert("refresh".into(), Value::Object(r.clone()));
        }
        Some(_) => return Err("refresh: an object (StartInstanceRefresh's Preferences)".into()),
    }
    Ok(vec![wave(object(EC2, "LaunchTemplate", template, None, json!({ "data": data })), 0), wave(object(ASG, "AutoScalingGroup", group, None, Value::Object(spec)), 1)])
}

// ------------------------------------------------------------------ objects → EC2's and Auto Scaling's requests

/// EC2's Query name for a list's items: `SecurityGroupIds` → `SecurityGroupId`.
fn item_name(list: &str) -> String {
    match list {
        "Groups" => "SecurityGroupId".to_owned(),
        "Tags" => "Tag".to_owned(),
        l if l.ends_with("ies") => format!("{}y", l.trim_end_matches("ies")),
        l if l.ends_with("Ids") || l.ends_with('s') => l.trim_end_matches('s').to_owned(),
        l => l.to_owned(),
    }
}

/// LaunchTemplateData as EC2's Query params take it: `LaunchTemplateData.InstanceType=t3.small`,
/// `LaunchTemplateData.SecurityGroupId.1=sg-1`, `…BlockDeviceMapping.1.Ebs.VolumeSize=20`.
pub fn data_params(prefix: &str, v: &Value, out: &mut Vec<(String, String)>) {
    match v {
        Value::Object(m) => {
            for (k, x) in m {
                match x {
                    Value::Array(items) => {
                        let name = item_name(k);
                        for (i, item) in items.iter().enumerate() {
                            data_params(&format!("{prefix}.{name}.{}", i + 1), item, out);
                        }
                    }
                    other => data_params(&format!("{prefix}.{k}"), other, out),
                }
            }
        }
        Value::String(s) => out.push((prefix.to_owned(), s.clone())),
        Value::Null => {}
        other => out.push((prefix.to_owned(), other.to_string())),
    }
}

/// CreateLaunchTemplate's (or CreateLaunchTemplateVersion's) params, its version described as
/// kiln's for `revision`.
pub fn template_params(desired: &Value, new_template: bool, revision: &str) -> Vec<(String, String)> {
    let mut p = vec![("LaunchTemplateName".to_owned(), crate::obj::name(desired).to_owned()), ("VersionDescription".to_owned(), format!("kiln {revision}"))];
    data_params("LaunchTemplateData", desired.pointer("/spec/data").unwrap_or(&Value::Null), &mut p);
    if new_template {
        let tags = super::aws_lambda::tags_of(&json!({ "metadata": desired.get("metadata").cloned().unwrap_or(Value::Null) }));
        if !tags.is_empty() {
            p.push(("TagSpecification.1.ResourceType".to_owned(), "launch-template".to_owned()));
            for (i, (k, v)) in tags.iter().enumerate() {
                p.push((format!("TagSpecification.1.Tag.{}.Key", i + 1), k.clone()));
                p.push((format!("TagSpecification.1.Tag.{}.Value", i + 1), v.as_str().unwrap_or_default().to_owned()));
            }
        }
    }
    p
}

/// The hash kiln keeps of what it made a version from.
pub fn data_hash(desired: &Value) -> String {
    format!("{:016x}", crate::obj::hash_value(desired.pointer("/spec/data").unwrap_or(&Value::Null)))
}

/// A group's settings as Auto Scaling's params take them (`TargetGroupARNs.member.1`).
fn group_settings(desired: &Value, out: &mut Vec<(String, String)>) {
    let spec = desired.get("spec").cloned().unwrap_or(Value::Null);
    for k in GROUP_SETTINGS {
        match spec.get(*k) {
            Some(Value::Array(items)) => {
                for (i, it) in items.iter().enumerate() {
                    out.push((format!("{k}.member.{}", i + 1), it.as_str().map_or_else(|| it.to_string(), str::to_owned)));
                }
            }
            Some(Value::String(s)) => out.push(((*k).to_owned(), s.clone())),
            Some(Value::Null) | None => {}
            Some(other) => out.push(((*k).to_owned(), other.to_string())),
        }
    }
}

/// CreateAutoScalingGroup's params, running `version` of its launch template.
pub fn create_group_params(desired: &Value, version: &str) -> Vec<(String, String)> {
    let mut p = vec![
        ("AutoScalingGroupName".to_owned(), crate::obj::name(desired).to_owned()),
        ("LaunchTemplate.LaunchTemplateName".to_owned(), crate::obj::str_at(desired, &["spec", "launchTemplate", "name"]).unwrap_or_default().to_owned()),
        ("LaunchTemplate.Version".to_owned(), version.to_owned()),
    ];
    group_settings(desired, &mut p);
    if !p.iter().any(|(k, _)| k == "MinSize") {
        p.push(("MinSize".to_owned(), "1".to_owned()));
    }
    if !p.iter().any(|(k, _)| k == "MaxSize") {
        p.push(("MaxSize".to_owned(), "1".to_owned()));
    }
    let tags = super::aws_lambda::tags_of(&json!({ "metadata": desired.get("metadata").cloned().unwrap_or(Value::Null) }));
    for (i, (k, v)) in tags.iter().enumerate() {
        p.push((format!("Tags.member.{}.Key", i + 1), k.clone()));
        p.push((format!("Tags.member.{}.Value", i + 1), v.as_str().unwrap_or_default().to_owned()));
        p.push((format!("Tags.member.{}.PropagateAtLaunch", i + 1), "true".to_owned()));
    }
    p
}

/// UpdateAutoScalingGroup's params: the settings kiln sets.
pub fn update_group_params(desired: &Value) -> Vec<(String, String)> {
    let mut p = vec![("AutoScalingGroupName".to_owned(), crate::obj::name(desired).to_owned())];
    group_settings(desired, &mut p);
    p.retain(|(k, _)| !k.starts_with("TargetGroupARNs"));
    p
}

/// StartInstanceRefresh's params: a rolling refresh onto `version`, with the group's
/// preferences; the group's launch template is set as it completes.
pub fn refresh_params(desired: &Value, version: &str) -> Vec<(String, String)> {
    let mut p = vec![
        ("AutoScalingGroupName".to_owned(), crate::obj::name(desired).to_owned()),
        ("Strategy".to_owned(), "Rolling".to_owned()),
        ("DesiredConfiguration.LaunchTemplate.LaunchTemplateName".to_owned(), crate::obj::str_at(desired, &["spec", "launchTemplate", "name"]).unwrap_or_default().to_owned()),
        ("DesiredConfiguration.LaunchTemplate.Version".to_owned(), version.to_owned()),
    ];
    let prefs = desired.pointer("/spec/refresh").and_then(Value::as_object).cloned().unwrap_or_default();
    for (k, v) in &prefs {
        match v {
            Value::Array(items) => {
                for (i, it) in items.iter().enumerate() {
                    p.push((format!("Preferences.{k}.member.{}", i + 1), it.as_str().map_or_else(|| it.to_string(), str::to_owned)));
                }
            }
            Value::String(s) => p.push((format!("Preferences.{k}"), s.clone())),
            other => p.push((format!("Preferences.{k}"), other.to_string())),
        }
    }
    p
}

// ------------------------------------------------------------------ the answers → objects

/// A LaunchTemplate object from one of DescribeLaunchTemplates' templates: what kiln made it from
/// (`made`: the desired object it last applied, and its version) while that version is the
/// default; another default, made so by anyone else, reads as that version (drift).
pub fn live_template(t: &Value, made: Option<(&Value, &str)>) -> Option<Value> {
    let name = t.get("launchTemplateName")?.as_str()?;
    let latest = t.get("latestVersionNumber").and_then(Value::as_str).unwrap_or_default();
    let default = t.get("defaultVersionNumber").and_then(Value::as_str).unwrap_or_default();
    let spec = match made {
        Some((desired, version)) if version == default => desired.get("spec").cloned().unwrap_or(Value::Null),
        _ => json!({ "data": { "version": default, "madeBy": "someone else" } }),
    };
    let mut o = object(EC2, "LaunchTemplate", name, None, spec);
    if let Value::Object(m) = &mut o {
        m.insert("status".into(), json!({ "id": t.get("launchTemplateId"), "latestVersion": latest, "defaultVersion": t.get("defaultVersionNumber") }));
    }
    Some(o)
}

/// An AutoScalingGroup object from one of DescribeAutoScalingGroups' groups, with its newest
/// instance refresh; `kilns` is the template's version kiln made, which reads as [`KILNS`].
pub fn live_group(g: &Value, refresh: Option<&Value>, kilns: Option<&str>) -> Option<Value> {
    let name = g.get("AutoScalingGroupName")?.as_str()?;
    let lt = g.get("LaunchTemplate").cloned().unwrap_or(Value::Null);
    let version = lt.get("Version").and_then(Value::as_str).unwrap_or_default();
    let shown = if kilns == Some(version) { KILNS.to_owned() } else { version.to_owned() };
    let mut spec = Map::new();
    spec.insert("launchTemplate".into(), json!({ "name": lt.get("LaunchTemplateName"), "version": shown }));
    for k in ["MinSize", "MaxSize", "DesiredCapacity", "HealthCheckType", "HealthCheckGracePeriod", "DefaultInstanceWarmup", "VPCZoneIdentifier"] {
        if let Some(v) = g.get(k).filter(|v| v.as_str().is_some_and(|s| !s.is_empty())) {
            let v = v.as_str().and_then(|s| s.parse::<i64>().ok().map(Value::from)).unwrap_or_else(|| v.clone());
            spec.insert(k.into(), v);
        }
    }
    let instances: Vec<Value> = g
        .get("Instances")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .map(|i| json!({ "id": i.get("InstanceId"), "state": i.get("LifecycleState"), "health": i.get("HealthStatus"), "version": i.pointer("/LaunchTemplate/Version") }))
        .collect();
    let mut o = object(ASG, "AutoScalingGroup", name, None, Value::Object(spec));
    if let Value::Object(m) = &mut o {
        let r = refresh.map(|r| json!({ "id": r.get("InstanceRefreshId"), "status": r.get("Status"), "reason": r.get("StatusReason"), "percent": r.get("PercentageComplete"), "version": r.pointer("/DesiredConfiguration/LaunchTemplate/Version") }));
        m.insert("status".into(), json!({ "version": version, "desired": g.get("DesiredCapacity"), "instances": instances, "refresh": r }));
    }
    Some(o)
}

/// Whether the group's settings kiln sets are as kiln wants (the template's version aside).
pub fn settings_match(desired: &Value, live: &Value) -> bool {
    let spec = desired.get("spec").cloned().unwrap_or(Value::Null);
    ["MinSize", "MaxSize", "DesiredCapacity", "HealthCheckType", "HealthCheckGracePeriod", "DefaultInstanceWarmup", "VPCZoneIdentifier"]
        .iter()
        .all(|k| match spec.get(*k) {
            None => true,
            Some(d) => live.pointer(&format!("/spec/{k}")).is_some_and(|l| super::aws_lambda::covers(d, l)),
        })
}

// ------------------------------------------------------------------ health

pub fn health(obj: &Value) -> Option<Status> {
    match crate::obj::kind(obj) {
        "LaunchTemplate" => status(Health::Healthy, ""),
        "AutoScalingGroup" => {
            let refresh = obj.pointer("/status/refresh").filter(|r| !r.is_null());
            if let Some(r) = refresh {
                let s = r.get("status").and_then(Value::as_str).unwrap_or_default();
                let reason = r.get("reason").and_then(Value::as_str).unwrap_or_default();
                let percent = r.get("percent").and_then(|p| p.as_str().map(str::to_owned).or_else(|| p.as_i64().map(|n| n.to_string()))).unwrap_or_default();
                match s {
                    "Pending" | "InProgress" | "Baking" | "Cancelling" | "RollbackInProgress" => {
                        let what = if s == "RollbackInProgress" { "rolling back" } else { "refreshing" };
                        let at = if percent.is_empty() { String::new() } else { format!(": {percent}%") };
                        return status(if s == "RollbackInProgress" { Health::Degraded } else { Health::Progressing }, format!("{what}{at}"));
                    }
                    "Failed" | "Cancelled" | "RollbackSuccessful" | "RollbackFailed" => {
                        // Once the group runs what kiln asks for (a rollback to what was put back),
                        // a refresh that failed is history.
                        if crate::obj::str_at(obj, &["spec", "launchTemplate", "version"]) != Some(KILNS) {
                            return status(Health::Degraded, if reason.is_empty() { format!("the instance refresh {}", s.to_ascii_lowercase()) } else { format!("the instance refresh {}: {reason}", s.to_ascii_lowercase()) });
                        }
                    }
                    _ => {}
                }
            }
            let instances = obj.pointer("/status/instances").and_then(Value::as_array).cloned().unwrap_or_default();
            let want = obj.pointer("/status/desired").and_then(|d| d.as_str().and_then(|s| s.parse::<usize>().ok()).or_else(|| d.as_u64().and_then(|n| usize::try_from(n).ok()))).unwrap_or(instances.len());
            let good = instances.iter().filter(|i| i.get("state").and_then(Value::as_str) == Some("InService") && i.get("health").and_then(Value::as_str) == Some("Healthy")).count();
            if good < want {
                return status(Health::Progressing, format!("{good} of {want} instances in service"));
            }
            status(Health::Healthy, format!("{good} of {want} instances in service"))
        }
        "Instance" => match crate::obj::str_at(obj, &["status", "state"]) {
            Some("InService") => status(Health::Healthy, ""),
            Some(s) => status(Health::Progressing, s),
            None => None,
        },
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn render_api(extra: Value) -> Result<Vec<Value>, String> {
        let mut spec = json!({ "group": "shop-api", "launch-template": "shop-api", "image": "ami-0abc", "template": { "InstanceType": "t3.small", "SecurityGroupIds": ["sg-1", "sg-2"], "BlockDeviceMappings": [{ "DeviceName": "/dev/xvda", "Ebs": { "VolumeSize": 20 } }] }, "group-spec": { "MinSize": 2, "MaxSize": 6, "VPCZoneIdentifier": "subnet-1,subnet-2", "Instances": [] }, "refresh": { "MinHealthyPercentage": 90, "CheckpointPercentages": [50], "AutoRollback": true } });
        if let (Value::Object(m), Value::Object(e)) = (&mut spec, extra) {
            m.extend(e);
        }
        let none = Map::new();
        render(&RenderInput { spec: &spec, target: &none, app: "api", namespace: "", revision: "abc", read: &|_| None })
    }

    #[test]
    fn a_definition_renders_a_template_and_the_group_that_runs_kiln_s_version() -> Result<(), String> {
        let objs = render_api(json!({}))?;
        let (lt, asg) = (objs.first().ok_or("template")?, objs.get(1).ok_or("group")?);
        assert_eq!(lt.pointer("/spec/data/ImageId"), Some(&json!("ami-0abc")));
        assert_eq!(asg.pointer("/spec/launchTemplate"), Some(&json!({ "name": "shop-api", "version": KILNS })));
        assert_eq!(asg.pointer("/spec/Instances"), None, "only a group's settings");
        let p = template_params(lt, true, "abc");
        let get = |k: &str| p.iter().find(|(x, _)| x == k).map(|(_, v)| v.as_str());
        assert_eq!(get("LaunchTemplateData.SecurityGroupId.2"), Some("sg-2"), "EC2's names for list items");
        assert_eq!(get("LaunchTemplateData.BlockDeviceMapping.1.Ebs.VolumeSize"), Some("20"));
        assert_eq!(get("LaunchTemplateData.ImageId"), Some("ami-0abc"));
        let r = refresh_params(asg, "4");
        assert!(r.contains(&("DesiredConfiguration.LaunchTemplate.Version".to_owned(), "4".to_owned())) && r.contains(&("Preferences.CheckpointPercentages.member.1".to_owned(), "50".to_owned())) && r.contains(&("Preferences.AutoRollback".to_owned(), "true".to_owned())), "{r:?}");
        let c = create_group_params(asg, "1");
        assert!(c.contains(&("VPCZoneIdentifier".to_owned(), "subnet-1,subnet-2".to_owned())) && c.contains(&("MinSize".to_owned(), "2".to_owned())));
        assert!(render_api(json!({ "image": "debian" })).is_err_and(|e| e.contains("AMI")));
        assert!(render_api(json!({ "image": null, "template": { "InstanceType": "t3.small" } })).is_err_and(|e| e.contains("needs an image")));
        assert_ne!(data_hash(lt), data_hash(&render_api(json!({ "image": "ami-0def" }))?.into_iter().next().unwrap_or_default()));
        Ok(())
    }

    #[test]
    fn groups_read_back_and_health_follows_their_refresh() -> Result<(), String> {
        let objs = render_api(json!({}))?;
        let (lt, asg) = (objs.first().ok_or("template")?, objs.get(1).ok_or("group")?);
        let t = json!({ "launchTemplateName": "shop-api", "launchTemplateId": "lt-1", "latestVersionNumber": "3", "defaultVersionNumber": "3" });
        assert_eq!(live_template(&t, Some((lt, "3"))).and_then(|o| o.get("spec").cloned()), lt.get("spec").cloned(), "kiln's version, newest: as kiln made it");
        assert_eq!(live_template(&t, Some((lt, "2"))).and_then(|o| o.pointer("/spec/data/madeBy").cloned()), Some(json!("someone else")), "a newer one is drift");
        let inst = |state: &str| json!({ "InstanceId": "i-1", "LifecycleState": state, "HealthStatus": "Healthy", "LaunchTemplate": { "Version": "3" } });
        let g = json!({ "AutoScalingGroupName": "shop-api", "LaunchTemplate": { "LaunchTemplateName": "shop-api", "Version": "3" }, "MinSize": "2", "MaxSize": "6", "DesiredCapacity": "2", "VPCZoneIdentifier": "subnet-1,subnet-2", "Instances": [inst("InService"), inst("InService")] });
        let live = live_group(&g, None, Some("3")).ok_or("group")?;
        assert_eq!(live.pointer("/spec/launchTemplate/version"), Some(&json!(KILNS)));
        assert!(settings_match(asg, &live), "{live}");
        assert_eq!(health(&live).map(|h| h.health), Some(Health::Healthy));
        let other = live_group(&g, None, Some("4")).ok_or("group")?;
        assert_eq!(other.pointer("/spec/launchTemplate/version"), Some(&json!("3")), "on another version than kiln's: drift");
        let refreshing = live_group(&g, Some(&json!({ "InstanceRefreshId": "r1", "Status": "InProgress", "PercentageComplete": "50", "DesiredConfiguration": { "LaunchTemplate": { "Version": "4" } } })), Some("4")).ok_or("group")?;
        assert_eq!(health(&refreshing).map(|h| (h.health, h.message)), Some((Health::Progressing, "refreshing: 50%".to_owned())));
        let failed = live_group(&g, Some(&json!({ "InstanceRefreshId": "r1", "Status": "RollbackSuccessful", "StatusReason": "the alarm went off", "DesiredConfiguration": { "LaunchTemplate": { "Version": "4" } } })), Some("4")).ok_or("group")?;
        assert_eq!(health(&failed).map(|h| h.health), Some(Health::Degraded));
        // Rolled back to, and kiln asks for it now: history, not health.
        let back = live_group(&g, Some(&json!({ "Status": "RollbackSuccessful", "DesiredConfiguration": { "LaunchTemplate": { "Version": "4" } } })), Some("3")).ok_or("group")?;
        assert_eq!(health(&back).map(|h| h.health), Some(Health::Healthy));
        Ok(())
    }
}
