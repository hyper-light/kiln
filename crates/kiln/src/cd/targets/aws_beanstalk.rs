//! `aws-beanstalk`'s I/O half (docs/cd-targets.md §3): Elastic Beanstalk's Query API. The pure
//! half, `kiln_cd::providers::aws_beanstalk`, reads its answers into objects and builds its
//! requests.
//!
//! An application is made if it isn't there. A version is made once: its bundle (a zip CI built
//! goes to kiln's bucket in the account first, under its hash) given to Beanstalk to process, and
//! kiln waits until it has; a bundle it can't process fails the apply. An environment is made, or
//! updated to the version and the options kiln sets, once it's Ready: one Beanstalk is updating is
//! left until it's done, and the next sync tries again. Its rollout is Beanstalk's own, by the
//! deployment policy its options name, and its health Beanstalk's.

use std::time::Duration;

use anyhow::{Result, bail};
use kiln_cd::obj::{self, ResKey};
use kiln_cd::providers::aws_beanstalk::{self as p, GROUP};
use serde_json::{Value, json};

use super::{Cx, applied, until};
use crate::ci::integrations::aws::{self, AwsError};

const VERSION: &str = "2010-12-01";
/// How long Beanstalk may take to process a bundle.
const PROCESS_LIMIT: Duration = Duration::from_secs(300);

/// An action's result, as JSON.
async fn call(cx: &Cx<'_>, action: &str, params: &[(String, String)]) -> Result<Value> {
    cx.conn.budget().await;
    let xml = cx.aws()?.query("elasticbeanstalk", action, VERSION, params).await?;
    let v = aws::xml_json(&xml);
    Ok(v.pointer(&format!("/{action}Response/{action}Result")).cloned().unwrap_or(Value::Null))
}

fn pair(k: &str, v: &str) -> (String, String) {
    (k.to_owned(), v.to_owned())
}

fn items(v: &Value, key: &str) -> Vec<Value> {
    v.get(key).and_then(Value::as_array).cloned().unwrap_or_default()
}

/// Beanstalk says something isn't there in an `InvalidParameterValue`.
fn missing(e: &anyhow::Error) -> bool {
    AwsError::is_not_found(e) || e.downcast_ref::<AwsError>().is_some_and(|a| a.code == "InvalidParameterValue" && (a.message.starts_with("No Application") || a.message.starts_with("No Environment") || a.message.contains("not found")))
}

async fn application(cx: &Cx<'_>, name: &str) -> Result<Option<Value>> {
    let v = call(cx, "DescribeApplications", &[pair("ApplicationNames.member.1", name)]).await?;
    Ok(items(&v, "Applications").first().and_then(p::live_application))
}

async fn version(cx: &Cx<'_>, label: &str) -> Result<Option<Value>> {
    let v = call(cx, "DescribeApplicationVersions", &[pair("VersionLabels.member.1", label)]).await?;
    Ok(items(&v, "ApplicationVersions").first().and_then(p::live_version))
}

/// The environment as DescribeEnvironments answers it (not one that's terminated).
async fn describe_environment(cx: &Cx<'_>, name: &str) -> Result<Option<Value>> {
    let v = call(cx, "DescribeEnvironments", &[pair("EnvironmentNames.member.1", name), pair("IncludeDeleted", "false")]).await?;
    Ok(items(&v, "Environments").into_iter().find(|e| e.get("Status").and_then(Value::as_str) != Some("Terminated")))
}

/// The environment as an object: the options kiln sets read back, its latest events.
async fn environment(cx: &Cx<'_>, name: &str) -> Result<Option<Value>> {
    let Some(e) = describe_environment(cx, name).await? else { return Ok(None) };
    let app = e.get("ApplicationName").and_then(Value::as_str).unwrap_or_default().to_owned();
    let settings = call(cx, "DescribeConfigurationSettings", &[pair("ApplicationName", &app), pair("EnvironmentName", name)]).await?;
    let settings = settings.pointer("/ConfigurationSettings/0/OptionSettings").and_then(Value::as_array).cloned().unwrap_or_default();
    let events = items(&call(cx, "DescribeEvents", &[pair("EnvironmentName", name), pair("MaxRecords", "10")]).await?, "Events");
    let wanted = cx.applied(GROUP, "Environment", "", name).and_then(|a| a.object.pointer("/spec/options").and_then(Value::as_array).cloned()).unwrap_or_default();
    Ok(p::live_environment(&e, &settings, &events, &wanted))
}

pub async fn list(cx: &Cx<'_>, kind: &str, names: &[(String, String)]) -> Result<Vec<Value>> {
    let mut out = Vec::new();
    for (_, name) in names {
        let o = match kind {
            "Application" => application(cx, name).await?,
            "ApplicationVersion" => version(cx, name).await?,
            "Environment" => environment(cx, name).await?,
            _ => None,
        };
        out.extend(o);
    }
    Ok(out)
}

/// What the account holds in the region: its applications and environments (§8.3).
pub async fn discover(cx: &Cx<'_>) -> Result<Value> {
    let apps: Vec<String> = items(&call(cx, "DescribeApplications", &[]).await?, "Applications").iter().filter_map(|a| a.get("ApplicationName").and_then(Value::as_str).map(str::to_owned)).collect();
    let resources: Vec<Value> = items(&call(cx, "DescribeEnvironments", &[pair("IncludeDeleted", "false")]).await?, "Environments")
        .iter()
        .map(|e| {
            let g = |k: &str| e.get(k).and_then(Value::as_str).unwrap_or_default();
            json!({ "kind": "Environment", "scope": g("ApplicationName"), "name": g("EnvironmentName"), "summary": format!("{} · {} · {}", g("Status"), g("Health"), g("VersionLabel")) })
        })
        .collect();
    Ok(json!({ "scopes": apps, "resources": resources }))
}

/// An environment as it runs now, as the definition that adopts it (§8.2 step 2): its
/// application, platform and the options it was given (Beanstalk's defaults aside); the bundle is
/// CI's to build.
pub async fn adopt(cx: &Cx<'_>, name: &str) -> Result<Value> {
    let Some(e) = describe_environment(cx, name).await? else { bail!("no environment {name}") };
    let g = |k: &str| e.get(k).and_then(Value::as_str).unwrap_or_default().to_owned();
    let platform = if g("PlatformArn").is_empty() { g("SolutionStackName") } else { g("PlatformArn") };
    Ok(json!({ "definition": { "application": g("ApplicationName"), "environment": name, "platform": platform, "bundle": "${{ artifacts.bundle }}" }, "note": format!("It runs {} now; give the bundle CI builds as an artifact.", g("VersionLabel")) }))
}

pub async fn apply(cx: &Cx<'_>, desired: &Value) -> Result<Value> {
    match obj::kind(desired) {
        "Application" => apply_application(cx, desired).await,
        "ApplicationVersion" => apply_version(cx, desired).await,
        "Environment" => apply_environment(cx, desired).await,
        other => bail!("aws-beanstalk doesn't apply {other}"),
    }
}

async fn apply_application(cx: &Cx<'_>, desired: &Value) -> Result<Value> {
    let name = obj::name(desired);
    if application(cx, name).await?.is_none() {
        call(cx, "CreateApplication", &p::create_application_params(desired)).await?;
    }
    cx.record(GROUP, "Application", "", name, Some(applied(desired, json!({})))).await?;
    application(cx, name).await?.ok_or_else(|| anyhow::anyhow!("application {name} is gone"))
}

async fn apply_version(cx: &Cx<'_>, desired: &Value) -> Result<Value> {
    let label = obj::name(desired);
    if version(cx, label).await?.is_none() {
        let bundle = desired.pointer("/spec/bundle").cloned().unwrap_or(Value::Null);
        let (bucket, key) = match bundle.get("Artifact").and_then(Value::as_str) {
            Some(a) => super::aws_s3::artifact(cx, cx.files, "beanstalk", a.trim_start_matches("sha256:"), ".zip").await?,
            None => (bundle.get("S3Bucket").and_then(Value::as_str).unwrap_or_default().to_owned(), bundle.get("S3Key").and_then(Value::as_str).unwrap_or_default().to_owned()),
        };
        match call(cx, "CreateApplicationVersion", &p::create_version_params(desired, &bucket, &key)).await {
            Ok(_) => {}
            // Made meanwhile (a sync retried): that one.
            Err(e) if e.downcast_ref::<AwsError>().is_some_and(|a| a.message.contains("already exists")) => {}
            Err(e) => return Err(e),
        }
    }
    let processed = until(cx.conn, PROCESS_LIMIT, &format!("Beanstalk processing version {label}"), || async {
        let Some(v) = version(cx, label).await? else { bail!("version {label} is gone") };
        Ok(match obj::str_at(&v, &["status", "status"]) {
            Some("PROCESSED" | "UNPROCESSED") => Some(Ok(v)),
            Some("FAILED") => Some(Err(())),
            _ => None,
        })
    })
    .await?;
    let Ok(live) = processed else { bail!("Beanstalk couldn't process version {label}'s bundle: see the environment's events") };
    cx.record(GROUP, "ApplicationVersion", "", label, Some(applied(desired, json!({})))).await?;
    Ok(live)
}

async fn apply_environment(cx: &Cx<'_>, desired: &Value) -> Result<Value> {
    let name = obj::name(desired);
    match environment(cx, name).await? {
        None => {
            let params = p::create_environment_params(desired).map_err(anyhow::Error::msg)?;
            call(cx, "CreateEnvironment", &params).await?;
        }
        Some(live) => {
            let state = obj::str_at(&live, &["status", "status"]).unwrap_or_default();
            if state != "Ready" {
                bail!("environment {name} is {state}: kiln changes it once Beanstalk is done");
            }
            let fixed = p::immutable_changes(desired, &live);
            if !fixed.is_empty() {
                bail!("environment {name} can't change {} in place: name a new environment, or terminate this one first", fixed.join(", "));
            }
            // Record first: what's read back of the options is what kiln sets.
            cx.record(GROUP, "Environment", "", name, Some(applied(desired, json!({ "id": live.pointer("/status/id") })))).await?;
            let live = environment(cx, name).await?.unwrap_or(live);
            if !p::environment_matches(desired, &live) {
                call(cx, "UpdateEnvironment", &p::update_environment_params(desired, &live)).await?;
            }
        }
    }
    cx.record(GROUP, "Environment", "", name, Some(applied(desired, json!({})))).await?;
    environment(cx, name).await?.ok_or_else(|| anyhow::anyhow!("environment {name} is gone"))
}

pub async fn delete(cx: &Cx<'_>, key: &ResKey) -> Result<()> {
    let r = match &*key.kind {
        "Environment" => call(cx, "TerminateEnvironment", &[pair("EnvironmentName", &key.name)]).await.map(|_| ()),
        "ApplicationVersion" => {
            let Some(v) = version(cx, &key.name).await? else { return Ok(()) };
            let app = obj::str_at(&v, &["spec", "application"]).unwrap_or_default().to_owned();
            call(cx, "DeleteApplicationVersion", &[pair("ApplicationName", &app), pair("VersionLabel", &key.name), pair("DeleteSourceBundle", "false")]).await.map(|_| ())
        }
        "Application" => call(cx, "DeleteApplication", &[pair("ApplicationName", &key.name), pair("TerminateEnvByForce", "false")]).await.map(|_| ()),
        other => bail!("aws-beanstalk doesn't delete {other}"),
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
    use crate::cd::targets::fake::{self, Fake, MemFiles, MemRecords};
    use crate::cd::targets::fake_query::{self as fq, FakeQuery, released};
    use crate::cd::targets::{Conn, Creds, key_of};
    use crate::ci::integrations::aws::{Aws, Credentials};

    fn spec() -> Value {
        json!({ "application": "shop", "environment": "shop-${{ env.name }}", "bundle": "${{ release.artifacts.bundle.digest }}", "platform": "64bit Amazon Linux 2023 v4.3.0 running Docker", "cname": "shop-staging", "options": { "aws:elasticbeanstalk:command": { "DeploymentPolicy": "Rolling", "BatchSize": 50 } } })
    }

    /// An app on a faked Beanstalk, release by release: the first makes its application, a
    /// version of its bundle (uploaded to kiln's bucket once) and its environment, which becomes
    /// Ready and Green; the same bundle again makes nothing; the next makes a version and updates
    /// the environment to it, Rolling as its options say; one that crashes leaves it Red, read as
    /// Degraded; the release before, rolled back to, uploads nothing and makes no version, and the
    /// environment is Green on it again; a bundle Beanstalk can't process fails; then it all goes.
    #[test]
    fn environments_run_each_releases_version() -> anyhow::Result<()> {
        let _one = fake::LAMBDA.lock();
        crate::rt::block_on(async {
            let (aws_fake, eb) = (Arc::new(Fake::default()), Arc::new(FakeQuery::default()));
            let (base, eb_base) = (fake::start(aws_fake.clone()).await?, fq::start(eb.clone()).await?);
            for service in ["s3", "sts"] {
                aws::override_endpoint(service, &base);
            }
            aws::override_endpoint("elasticbeanstalk", &eb_base);
            let target = Target::parse("aws-beanstalk/aws-test/us-east-1").ok_or_else(|| anyhow::anyhow!("target"))?;
            let def = target.def().ok_or_else(|| anyhow::anyhow!("provider"))?;
            assert!(def.built);
            let conn = Conn::quick(target, def);
            let (records, files) = (MemRecords::default(), MemFiles::default());
            let creds = Credentials { access_key_id: "AKIDFAKE".into(), secret_access_key: "s".into(), session_token: None, expires_at: None };
            let cx = Cx { conn: &conn, creds: Creds::Aws(Aws { creds, region: "us-east-1".into() }), records: &records, files: &files };
            let names = [(String::new(), "shop-staging".to_owned())];
            let settled = || async {
                for _ in 0..10 {
                    let live = list(&cx, "Environment", &names).await?;
                    if live.first().is_some_and(|l| obj::str_at(l, &["status", "status"]) == Some("Ready")) {
                        return Ok::<_, anyhow::Error>(cx.with_applied(live.into_iter().next().unwrap_or_default()));
                    }
                }
                anyhow::bail!("the environment didn't settle")
            };
            let release = |version: &str, bundle: &str| -> anyhow::Result<Vec<Value>> {
                let digest = format!("sha256:{}", files.add(bundle.as_bytes()));
                released("aws-beanstalk", spec(), version, &[], &[("bundle", &digest)])
            };
            let cxr = &cx;
            let apply_all = |objs: Vec<Value>| async move {
                for o in &objs {
                    apply(cxr, o).await?;
                }
                Ok::<_, anyhow::Error>(objs)
            };

            // The first release: its application, its version, its environment.
            let first = apply_all(release("1.0.0", "zip of web 1.0.0")?).await?;
            let label1 = obj::name(first.get(1).unwrap_or(&Value::Null)).to_owned();
            let up = settled().await?;
            assert_eq!(kiln_cd::providers::health(&up).map(|h| h.health), Some(Health::Healthy), "{up}");
            assert!(p::environment_matches(first.get(2).unwrap_or(&Value::Null), &up), "the options kiln sets read back: {up}");
            assert_eq!(up.pointer("/spec/versionLabel"), Some(&json!(label1)));
            let uploads = |a: &Fake| a.state.lock().calls.iter().filter(|c| c.starts_with("PutObject")).count();
            assert_eq!(uploads(&aws_fake), 1, "the bundle went to kiln's bucket");
            assert_eq!(eb.state.lock().calls, ["CreateApplication shop".to_owned(), format!("CreateApplicationVersion {label1}"), format!("CreateEnvironment shop-staging {label1}")]);

            // The same bundle again: nothing made, nothing uploaded.
            apply_all(release("1.0.0", "zip of web 1.0.0")?).await?;
            assert_eq!(eb.state.lock().calls.len(), 3);
            assert_eq!(uploads(&aws_fake), 1);

            // The next: a version, and the environment updated to it.
            let second = apply_all(release("1.1.0", "zip of web 1.1.0")?).await?;
            let label2 = obj::name(second.get(1).unwrap_or(&Value::Null)).to_owned();
            assert_ne!(label1, label2);
            let live = list(&cx, "Environment", &names).await?.into_iter().next().unwrap_or_default();
            assert_eq!(kiln_cd::providers::health(&live).map(|h| h.health), Some(Health::Progressing), "updating: {live}");
            let up2 = settled().await?;
            assert_eq!((up2.pointer("/spec/versionLabel"), kiln_cd::providers::health(&up2).map(|h| h.health)), (Some(&json!(label2)), Some(Health::Healthy)));
            assert!(up2.pointer("/status/events/0/message").and_then(Value::as_str).is_some_and(|m| m.contains("completed successfully")), "{up2}");
            assert_eq!(eb.state.lock().calls.last().cloned(), Some(format!("UpdateEnvironment shop-staging {label2}")));

            // One that crashes as it starts: Red, read as Degraded.
            let crash = files.add(b"zip of web 1.2.0");
            let crashed = released("aws-beanstalk", spec(), "1.2.0", &[], &[("bundle", &format!("sha256:{crash}"))])?;
            // Its bundle in kiln's bucket says crash: the fake's way to fail a start.
            let label3 = obj::name(crashed.get(1).unwrap_or(&Value::Null)).to_owned();
            apply(&cx, crashed.first().unwrap_or(&Value::Null)).await?;
            apply(&cx, crashed.get(1).unwrap_or(&Value::Null)).await?;
            if let Some(v) = eb.state.lock().versions.get_mut(&("shop".to_owned(), label3.clone())) {
                v.1 = format!("{}-crash", v.1);
            }
            apply(&cx, crashed.get(2).unwrap_or(&Value::Null)).await?;
            let red = settled().await?;
            assert_eq!(kiln_cd::providers::health(&red).map(|h| h.health), Some(Health::Degraded), "{red}");

            // Rolled back to the release before: its version is there, nothing uploaded or made.
            let (made, uploaded) = (eb.state.lock().calls.iter().filter(|c| c.starts_with("CreateApplicationVersion")).count(), uploads(&aws_fake));
            apply_all(release("1.1.0", "zip of web 1.1.0")?).await?;
            assert_eq!(eb.state.lock().calls.iter().filter(|c| c.starts_with("CreateApplicationVersion")).count(), made, "no version made for a rollback");
            assert_eq!(uploads(&aws_fake), uploaded, "nothing uploaded for a rollback");
            let back = settled().await?;
            assert_eq!((back.pointer("/spec/versionLabel"), kiln_cd::providers::health(&back).map(|h| h.health)), (Some(&json!(label2)), Some(Health::Healthy)));

            // A bundle Beanstalk can't process fails its apply.
            let broken = files.add(b"not a zip");
            let objs = released("aws-beanstalk", spec(), "1.3.0", &[], &[("bundle", &format!("sha256:{broken}"))])?;
            apply(&cx, objs.first().unwrap_or(&Value::Null)).await?;
            let label4 = obj::name(objs.get(1).unwrap_or(&Value::Null)).to_owned();
            eb.state.lock().versions.insert(("shop".to_owned(), label4.clone()), ("kiln-123456789012-us-east-1".into(), "kiln/beanstalk/broken.zip".into(), "PROCESSING".into(), 0));
            assert!(apply(&cx, objs.get(1).unwrap_or(&Value::Null)).await.is_err_and(|e| format!("{e:#}").contains("couldn't process")));

            // What the account holds; then it goes, the environment first.
            let found = discover(&cx).await?;
            assert_eq!(found.pointer("/resources/0/name"), Some(&json!("shop-staging")));
            let adopted = adopt(&cx, "shop-staging").await?;
            assert_eq!(adopted.pointer("/definition/application"), Some(&json!("shop")));
            delete(&cx, &key_of(second.get(2).unwrap_or(&Value::Null))).await?;
            let _ = settled().await;
            assert!(list(&cx, "Environment", &names).await?.is_empty(), "terminated");
            delete(&cx, &key_of(first.get(1).unwrap_or(&Value::Null))).await?;
            delete(&cx, &key_of(first.first().unwrap_or(&Value::Null))).await?;
            assert!(list(&cx, "Application", &[(String::new(), "shop".to_owned())]).await?.is_empty());
            Ok(())
        })
    }
}
