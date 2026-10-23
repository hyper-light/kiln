//! `aws-beanstalk` (docs/cd-targets.md §3): an application, its versions, and the environment
//! that runs one.
//!
//! ```yaml
//! web:
//!   aws-beanstalk:
//!     application: shop
//!     environment: shop-staging
//!     bundle: "${{ artifacts.bundle }}"     # a zip CI built, or s3://bucket/key
//!     platform: "64bit Amazon Linux 2023 v4.3.0 running Docker"   # or a platform's ARN
//!     cname: shop-staging                   # for an environment kiln makes
//!     options:                              # OptionSettings, by namespace
//!       aws:autoscaling:asg: { MinSize: 2, MaxSize: 4 }
//!       aws:elasticbeanstalk:command: { DeploymentPolicy: Rolling, BatchSize: 50 }
//! ```
//!
//! - **Application** (wave 0): made if it isn't there.
//! - **ApplicationVersion** (wave 1): named for its bundle (`kiln-<sha256's first 16>`), so the
//!   same bundle is the same version: a rollback makes nothing and uploads nothing. A zip CI built
//!   goes to kiln's bucket in the account first. Versions are immutable.
//! - **Environment** (wave 2): runs the version, with the options the definition sets (the rest
//!   are Beanstalk's own, or someone else's). Beanstalk rolls it out by its own deployment policy
//!   (`aws:elasticbeanstalk:command` `DeploymentPolicy`: all at once, rolling, rolling with an
//!   extra batch, immutable, traffic splitting), and its health follows Beanstalk's.

use serde_json::{Map, Value, json};

use super::{RenderInput, object, status, text};
use crate::health::Status;
use crate::model::Health;

pub const GROUP: &str = "elasticbeanstalk.aws.kiln.dev";

/// Where kiln keeps a bundle CI built, in its bucket: `kiln/beanstalk/<sha256>.zip`.
pub const BUNDLE_PREFIX: &str = "kiln/beanstalk/";

/// The files a render reads: none, the bundle is CI's.
pub fn files(_spec: &Value) -> Vec<String> {
    Vec::new()
}

/// A bundle as `bundle:` says it: `sha256:<hex>` (a zip CI built) or `s3://bucket/key`.
fn bundle_of(s: &str) -> Result<Value, String> {
    let s = s.trim();
    if let Some(hex) = s.strip_prefix("sha256:").filter(|h| h.len() == 64 && h.bytes().all(|b| b.is_ascii_hexdigit())) {
        return Ok(json!({ "Artifact": format!("sha256:{}", hex.to_ascii_lowercase()) }));
    }
    if let Some(rest) = s.strip_prefix("s3://")
        && let Some((bucket, key)) = rest.split_once('/').filter(|(b, k)| !b.is_empty() && !k.is_empty())
    {
        return Ok(json!({ "S3Bucket": bucket, "S3Key": key }));
    }
    Err(format!("`{s}` is no bundle Beanstalk takes: a zip CI built (${{{{ artifacts.<name> }}}}), or s3://bucket/key"))
}

/// A version's label for a bundle: the same bundle, the same version.
pub fn label_of(bundle: &Value) -> String {
    let key = match bundle.get("Artifact").and_then(Value::as_str) {
        Some(a) => a.trim_start_matches("sha256:").to_owned(),
        None => format!("{:016x}", crate::obj::hash_value(bundle)),
    };
    format!("kiln-{}", key.get(..16).unwrap_or(&key))
}

/// `options:` (by namespace, then name) as Beanstalk's OptionSettings, sorted.
fn options_of(spec: &Value) -> Result<Vec<Value>, String> {
    let mut out = Vec::new();
    match spec.get("options") {
        None | Some(Value::Null) => {}
        Some(Value::Object(namespaces)) => {
            for (ns, opts) in namespaces {
                let Value::Object(opts) = opts else { return Err(format!("options.{ns}: a map of option names to values")) };
                for (name, v) in opts {
                    let value = match v {
                        Value::String(s) => s.clone(),
                        Value::Number(_) | Value::Bool(_) => v.to_string(),
                        _ => return Err(format!("options.{ns}.{name}: text, a number or true/false")),
                    };
                    out.push(json!({ "Namespace": ns, "OptionName": name, "Value": value }));
                }
            }
        }
        Some(_) => return Err("options: a map of namespaces (aws:autoscaling:asg: { MinSize: 2 })".into()),
    }
    out.sort_by_key(|o| (o.get("Namespace").and_then(Value::as_str).unwrap_or_default().to_owned(), o.get("OptionName").and_then(Value::as_str).unwrap_or_default().to_owned()));
    Ok(out)
}

fn wave(mut o: Value, w: i64) -> Value {
    if let Some(ann) = crate::obj::ensure_map(&mut o, &["metadata", "annotations"]) {
        ann.insert(crate::sync::KILN_WAVE.into(), Value::String(w.to_string()));
    }
    o
}

/// An app's objects.
pub fn render(input: &RenderInput<'_>) -> Result<Vec<Value>, String> {
    let application = text(input.spec, "application").ok_or("aws-beanstalk needs application: the application's name")?;
    let environment = text(input.spec, "environment").ok_or("aws-beanstalk needs environment: the environment's name")?;
    if environment.len() < 4 || environment.len() > 40 || !environment.chars().all(|c| c.is_ascii_alphanumeric() || c == '-') || environment.starts_with('-') || environment.ends_with('-') {
        return Err(format!("environment `{environment}`: 4 to 40 letters, digits and -, not starting or ending with -"));
    }
    let bundle = bundle_of(text(input.spec, "bundle").ok_or("aws-beanstalk needs bundle: a zip CI built (${{ artifacts.<name> }}), or s3://bucket/key")?)?;
    let label = label_of(&bundle);
    let mut env = Map::new();
    env.insert("application".into(), Value::String(application.to_owned()));
    env.insert("versionLabel".into(), Value::String(label.clone()));
    if let Some(p) = text(input.spec, "platform") {
        let key = if p.starts_with("arn:") { "platformArn" } else { "solutionStack" };
        env.insert(key.into(), Value::String(p.to_owned()));
    }
    if let Some(t) = text(input.spec, "tier") {
        if !matches!(t, "WebServer" | "Worker") {
            return Err(format!("tier `{t}`: WebServer or Worker"));
        }
        env.insert("tier".into(), Value::String(t.to_owned()));
    }
    if let Some(c) = text(input.spec, "cname") {
        env.insert("cname".into(), Value::String(c.to_owned()));
    }
    let options = options_of(input.spec)?;
    if !options.is_empty() {
        env.insert("options".into(), Value::Array(options));
    }
    Ok(vec![
        wave(object(GROUP, "Application", application, None, json!({ "description": format!("{} (kiln)", input.app) })), 0),
        wave(object(GROUP, "ApplicationVersion", &label, None, json!({ "application": application, "bundle": bundle })), 1),
        wave(object(GROUP, "Environment", environment, None, Value::Object(env)), 2),
    ])
}

// ------------------------------------------------------------------ Beanstalk's answers → objects

/// An Application object from one of DescribeApplications' applications.
pub fn live_application(a: &Value) -> Option<Value> {
    let name = a.get("ApplicationName")?.as_str()?;
    let mut o = object(GROUP, "Application", name, None, json!({ "description": a.get("Description").and_then(Value::as_str).unwrap_or_default() }));
    if let Value::Object(m) = &mut o {
        m.insert("status".into(), json!({ "arn": a.get("ApplicationArn"), "versions": a.get("Versions").and_then(Value::as_array).map_or(0, Vec::len) }));
    }
    Some(o)
}

/// An ApplicationVersion object from one of DescribeApplicationVersions' versions; a bundle in
/// kiln's bucket reads as the artifact it holds.
pub fn live_version(v: &Value) -> Option<Value> {
    let label = v.get("VersionLabel")?.as_str()?;
    let bucket = v.pointer("/SourceBundle/S3Bucket").and_then(Value::as_str).unwrap_or_default();
    let key = v.pointer("/SourceBundle/S3Key").and_then(Value::as_str).unwrap_or_default();
    let bundle = match key.strip_prefix(BUNDLE_PREFIX).and_then(|k| k.strip_suffix(".zip")).filter(|_| bucket.starts_with("kiln-")) {
        Some(hex) => json!({ "Artifact": format!("sha256:{hex}") }),
        None => json!({ "S3Bucket": bucket, "S3Key": key }),
    };
    let mut o = object(GROUP, "ApplicationVersion", label, None, json!({ "application": v.get("ApplicationName"), "bundle": bundle }));
    if let Value::Object(m) = &mut o {
        m.insert("status".into(), json!({ "status": v.get("Status"), "created": v.get("DateCreated") }));
    }
    Some(o)
}

/// An Environment object from one of DescribeEnvironments' environments, with its configuration's
/// option settings (DescribeConfigurationSettings) and its latest events (DescribeEvents).
/// `wanted` is the options kiln sets: only those read back, the rest being Beanstalk's.
pub fn live_environment(e: &Value, settings: &[Value], events: &[Value], wanted: &[Value]) -> Option<Value> {
    let name = e.get("EnvironmentName")?.as_str()?;
    let get = |k: &str| e.get(k).and_then(Value::as_str).unwrap_or_default().to_owned();
    let mut spec = Map::new();
    spec.insert("application".into(), Value::String(get("ApplicationName")));
    spec.insert("versionLabel".into(), Value::String(get("VersionLabel")));
    if !get("PlatformArn").is_empty() {
        spec.insert("platformArn".into(), Value::String(get("PlatformArn")));
    }
    if !get("SolutionStackName").is_empty() {
        spec.insert("solutionStack".into(), Value::String(get("SolutionStackName")));
    }
    if let Some(t) = e.pointer("/Tier/Name").and_then(Value::as_str) {
        spec.insert("tier".into(), Value::String(t.to_owned()));
    }
    // The CNAME prefix: `shop-staging.<region>.elasticbeanstalk.com` → `shop-staging`.
    if let Some(prefix) = e.get("CNAME").and_then(Value::as_str).and_then(|c| c.split('.').next()).filter(|p| !p.is_empty()) {
        spec.insert("cname".into(), Value::String(prefix.to_owned()));
    }
    let key = |o: &Value| (o.get("Namespace").and_then(Value::as_str).unwrap_or_default().to_owned(), o.get("OptionName").and_then(Value::as_str).unwrap_or_default().to_owned());
    let mut options: Vec<Value> = settings
        .iter()
        .filter(|s| wanted.iter().any(|w| key(w) == key(s)))
        .map(|s| json!({ "Namespace": s.get("Namespace"), "OptionName": s.get("OptionName"), "Value": s.get("Value").and_then(Value::as_str).unwrap_or_default() }))
        .collect();
    options.sort_by_key(key);
    if !options.is_empty() {
        spec.insert("options".into(), Value::Array(options));
    }
    let mut o = object(GROUP, "Environment", name, None, Value::Object(spec));
    let events: Vec<Value> = events.iter().take(10).map(|ev| json!({ "at": ev.get("EventDate"), "severity": ev.get("Severity"), "message": ev.get("Message") })).collect();
    if let Value::Object(m) = &mut o {
        m.insert(
            "status".into(),
            json!({ "status": get("Status"), "health": get("Health"), "healthStatus": get("HealthStatus"), "url": get("CNAME"), "id": get("EnvironmentId"), "arn": get("EnvironmentArn"), "busy": e.get("AbortableOperationInProgress"), "events": events }),
        );
    }
    Some(o)
}

/// Whether the environment runs the version kiln wants with every option kiln sets.
pub fn environment_matches(desired: &Value, live: &Value) -> bool {
    let d = |k: &str| desired.pointer(&format!("/spec/{k}"));
    let l = |k: &str| live.pointer(&format!("/spec/{k}"));
    let options = |o: Option<&Value>| o.and_then(Value::as_array).cloned().unwrap_or_default();
    d("versionLabel") == l("versionLabel") && options(d("options")).iter().all(|w| options(l("options")).contains(w))
}

// ------------------------------------------------------------------ objects → Beanstalk's requests

fn options_params(options: &[Value]) -> Vec<(String, String)> {
    let mut out = Vec::new();
    for (i, o) in options.iter().enumerate() {
        let g = |k: &str| o.get(k).and_then(Value::as_str).unwrap_or_default().to_owned();
        out.push((format!("OptionSettings.member.{}.Namespace", i + 1), g("Namespace")));
        out.push((format!("OptionSettings.member.{}.OptionName", i + 1), g("OptionName")));
        out.push((format!("OptionSettings.member.{}.Value", i + 1), g("Value")));
    }
    out
}

fn tags_params(desired: &Value) -> Vec<(String, String)> {
    let tags = super::aws_lambda::tags_of(&json!({ "metadata": desired.get("metadata").cloned().unwrap_or(Value::Null) }));
    tags.iter().enumerate().flat_map(|(i, (k, v))| [(format!("Tags.member.{}.Key", i + 1), k.clone()), (format!("Tags.member.{}.Value", i + 1), v.as_str().unwrap_or_default().to_owned())]).collect()
}

/// CreateApplication's params.
pub fn create_application_params(desired: &Value) -> Vec<(String, String)> {
    let mut p = vec![("ApplicationName".to_owned(), crate::obj::name(desired).to_owned()), ("Description".to_owned(), crate::obj::str_at(desired, &["spec", "description"]).unwrap_or_default().to_owned())];
    p.extend(tags_params(desired));
    p
}

/// CreateApplicationVersion's params, its bundle at `bucket`/`key`.
pub fn create_version_params(desired: &Value, bucket: &str, key: &str) -> Vec<(String, String)> {
    vec![
        ("ApplicationName".to_owned(), crate::obj::str_at(desired, &["spec", "application"]).unwrap_or_default().to_owned()),
        ("VersionLabel".to_owned(), crate::obj::name(desired).to_owned()),
        ("SourceBundle.S3Bucket".to_owned(), bucket.to_owned()),
        ("SourceBundle.S3Key".to_owned(), key.to_owned()),
        ("Process".to_owned(), "true".to_owned()),
        ("Description".to_owned(), "kiln".to_owned()),
    ]
}

/// CreateEnvironment's params.
pub fn create_environment_params(desired: &Value) -> Result<Vec<(String, String)>, String> {
    let g = |k: &str| crate::obj::str_at(desired, &["spec", k]).map(str::to_owned);
    let mut p = vec![
        ("ApplicationName".to_owned(), g("application").unwrap_or_default()),
        ("EnvironmentName".to_owned(), crate::obj::name(desired).to_owned()),
        ("VersionLabel".to_owned(), g("versionLabel").unwrap_or_default()),
    ];
    match (g("platformArn"), g("solutionStack")) {
        (Some(arn), _) => p.push(("PlatformArn".to_owned(), arn)),
        (None, Some(stack)) => p.push(("SolutionStackName".to_owned(), stack)),
        (None, None) => return Err(format!("environment {} isn't there, and the definition names no platform to make it on (platform: a solution stack, or a platform's ARN)", crate::obj::name(desired))),
    }
    if let Some(t) = g("tier") {
        p.push(("Tier.Name".to_owned(), t.clone()));
        p.push(("Tier.Type".to_owned(), if t == "Worker" { "SQS/HTTP".to_owned() } else { "Standard".to_owned() }));
    }
    if let Some(c) = g("cname") {
        p.push(("CNAMEPrefix".to_owned(), c));
    }
    p.extend(options_params(desired.pointer("/spec/options").and_then(Value::as_array).map(Vec::as_slice).unwrap_or_default()));
    p.extend(tags_params(desired));
    Ok(p)
}

/// UpdateEnvironment's params: the version, and the options kiln sets that differ from `live`'s.
pub fn update_environment_params(desired: &Value, live: &Value) -> Vec<(String, String)> {
    let mut p = vec![("EnvironmentName".to_owned(), crate::obj::name(desired).to_owned()), ("VersionLabel".to_owned(), crate::obj::str_at(desired, &["spec", "versionLabel"]).unwrap_or_default().to_owned())];
    let have: Vec<Value> = live.pointer("/spec/options").and_then(Value::as_array).cloned().unwrap_or_default();
    let changed: Vec<Value> = desired.pointer("/spec/options").and_then(Value::as_array).into_iter().flatten().filter(|o| !have.contains(o)).cloned().collect();
    p.extend(options_params(&changed));
    p
}

/// Settings of the environment a change can't make in place: changing them means a new one.
pub fn immutable_changes(desired: &Value, live: &Value) -> Vec<&'static str> {
    ["tier", "cname"].into_iter().filter(|k| desired.pointer(&format!("/spec/{k}")).is_some_and(|d| live.pointer(&format!("/spec/{k}")).is_some_and(|l| l != d))).collect()
}

// ------------------------------------------------------------------ health

pub fn health(obj: &Value) -> Option<Status> {
    match crate::obj::kind(obj) {
        "Application" => status(Health::Healthy, ""),
        "ApplicationVersion" => match crate::obj::str_at(obj, &["status", "status"]) {
            Some("PROCESSED" | "UNPROCESSED") => status(Health::Healthy, ""),
            Some("FAILED") => status(Health::Degraded, "Beanstalk couldn't process the bundle"),
            Some(s) => status(Health::Progressing, s.to_ascii_lowercase()),
            None => None,
        },
        "Environment" => {
            let s = |k: &str| crate::obj::str_at(obj, &["status", k]).unwrap_or_default();
            let detail = if s("healthStatus").is_empty() { s("health").to_owned() } else { format!("{} ({})", s("health"), s("healthStatus")) };
            match s("status") {
                "Launching" => status(Health::Progressing, "launching"),
                "Updating" => status(Health::Progressing, "updating"),
                "Terminating" => status(Health::Progressing, "terminating"),
                "Terminated" => status(Health::Missing, "terminated"),
                "Ready" => match s("health") {
                    "Green" => status(Health::Healthy, detail),
                    "Red" => status(Health::Degraded, detail),
                    // Beanstalk's warning while a rolling update moves on: settle, or fail by time.
                    "Yellow" => status(Health::Progressing, detail),
                    _ => status(Health::Progressing, if detail.is_empty() { "waiting for health".to_owned() } else { detail }),
                },
                "" => None,
                other => status(Health::Progressing, other.to_ascii_lowercase()),
            }
        }
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const HEX: &str = "ab12cd34ef56ab12cd34ef56ab12cd34ef56ab12cd34ef56ab12cd34ef56ab12";

    fn render_web(extra: Value) -> Result<Vec<Value>, String> {
        let mut spec = json!({ "application": "shop", "environment": "shop-staging", "bundle": format!("sha256:{HEX}"), "platform": "64bit Amazon Linux 2023 v4.3.0 running Docker", "cname": "shop-staging", "options": { "aws:elasticbeanstalk:command": { "DeploymentPolicy": "Rolling", "BatchSize": 50 }, "aws:autoscaling:asg": { "MinSize": 2 } } });
        if let (Value::Object(m), Value::Object(e)) = (&mut spec, extra) {
            m.extend(e);
        }
        let none = Map::new();
        render(&RenderInput { spec: &spec, target: &none, app: "web", namespace: "", revision: "abc", read: &|_| None })
    }

    #[test]
    fn a_definition_renders_an_application_a_version_and_its_environment() -> Result<(), String> {
        let objs = render_web(json!({}))?;
        let kinds: Vec<&str> = objs.iter().map(crate::obj::kind).collect();
        assert_eq!(kinds, ["Application", "ApplicationVersion", "Environment"]);
        let waves: Vec<i64> = objs.iter().map(crate::sync::wave_of).collect();
        assert_eq!(waves, [0, 1, 2], "the application, then its version, then the environment");
        let version = objs.get(1).ok_or("version")?;
        assert_eq!(crate::obj::name(version), "kiln-ab12cd34ef56ab12", "named for its bundle");
        assert_eq!(version.pointer("/spec/bundle/Artifact"), Some(&json!(format!("sha256:{HEX}"))));
        let env = objs.get(2).ok_or("environment")?;
        assert_eq!(env.pointer("/spec/versionLabel"), Some(&json!("kiln-ab12cd34ef56ab12")));
        assert_eq!(env.pointer("/spec/options/0"), Some(&json!({ "Namespace": "aws:autoscaling:asg", "OptionName": "MinSize", "Value": "2" })), "sorted, as text");
        let params = create_environment_params(env)?;
        assert!(params.contains(&("SolutionStackName".to_owned(), "64bit Amazon Linux 2023 v4.3.0 running Docker".to_owned())));
        assert!(params.contains(&("OptionSettings.member.2.OptionName".to_owned(), "BatchSize".to_owned())), "{params:?}");
        assert!(render_web(json!({ "bundle": "the zip" })).is_err_and(|e| e.contains("no bundle")));
        assert!(render_web(json!({ "environment": "x" })).is_err(), "too short a name");
        let s3 = render_web(json!({ "bundle": "s3://builds/web/1.zip" }))?;
        assert!(s3.get(1).is_some_and(|v| crate::obj::name(v).starts_with("kiln-") && v.pointer("/spec/bundle/S3Key") == Some(&json!("web/1.zip"))));
        Ok(())
    }

    #[test]
    fn beanstalks_answers_read_as_objects_and_health_follows_its_own() -> Result<(), String> {
        let objs = render_web(json!({}))?;
        let (version, env) = (objs.get(1).ok_or("version")?, objs.get(2).ok_or("env")?);
        let live_v = live_version(&json!({ "ApplicationName": "shop", "VersionLabel": "kiln-ab12cd34ef56ab12", "SourceBundle": { "S3Bucket": "kiln-1-us-east-1", "S3Key": format!("{BUNDLE_PREFIX}{HEX}.zip") }, "Status": "PROCESSED" })).ok_or("live version")?;
        assert_eq!(live_v.pointer("/spec/bundle"), version.pointer("/spec/bundle"), "kiln's bucket's bundle reads as its artifact");
        let answer = json!({ "EnvironmentName": "shop-staging", "ApplicationName": "shop", "VersionLabel": "kiln-ab12cd34ef56ab12", "SolutionStackName": "64bit Amazon Linux 2023 v4.3.0 running Docker", "CNAME": "shop-staging.us-east-1.elasticbeanstalk.com", "Status": "Ready", "Health": "Green", "Tier": { "Name": "WebServer", "Type": "Standard" } });
        let settings = vec![
            json!({ "Namespace": "aws:autoscaling:asg", "OptionName": "MinSize", "Value": "2" }),
            json!({ "Namespace": "aws:autoscaling:asg", "OptionName": "MaxSize", "Value": "4" }),
            json!({ "Namespace": "aws:elasticbeanstalk:command", "OptionName": "BatchSize", "Value": "50" }),
            json!({ "Namespace": "aws:elasticbeanstalk:command", "OptionName": "DeploymentPolicy", "Value": "Rolling" }),
        ];
        let wanted = env.pointer("/spec/options").and_then(Value::as_array).cloned().unwrap_or_default();
        let live = live_environment(&answer, &settings, &[json!({ "Message": "Environment update completed successfully.", "Severity": "INFO" })], &wanted).ok_or("live env")?;
        assert!(environment_matches(env, &live), "{live}");
        assert_eq!(live.pointer("/spec/cname"), Some(&json!("shop-staging")));
        assert_eq!(live.pointer("/spec/options").and_then(Value::as_array).map(Vec::len), Some(3), "only kiln's options");
        assert_eq!(health(&live).map(|h| h.health), Some(Health::Healthy));
        let mut next = env.clone();
        crate::providers::set_path(&mut next, "spec.versionLabel", json!("kiln-ffff"))?;
        assert!(!environment_matches(&next, &live));
        let up = update_environment_params(&next, &live);
        assert!(up.contains(&("VersionLabel".to_owned(), "kiln-ffff".to_owned())) && !up.iter().any(|(k, _)| k.starts_with("OptionSettings")), "only what differs: {up:?}");
        let at = |s: &str, h: &str| {
            let mut o = live.clone();
            crate::providers::set_path(&mut o, "status.status", json!(s)).ok();
            crate::providers::set_path(&mut o, "status.health", json!(h)).ok();
            health(&o).map(|x| x.health)
        };
        assert_eq!(at("Updating", "Grey"), Some(Health::Progressing));
        assert_eq!(at("Ready", "Red"), Some(Health::Degraded));
        assert_eq!(at("Terminated", "Grey"), Some(Health::Missing));
        assert!(immutable_changes(&next, &live).is_empty());
        let mut moved = env.clone();
        crate::providers::set_path(&mut moved, "spec.cname", json!("other"))?;
        assert_eq!(immutable_changes(&moved, &live), ["cname"]);
        Ok(())
    }
}
