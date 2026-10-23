//! `cloudformation` (docs/cd-targets.md §3): a stack, changed through change sets.
//!
//! ```yaml
//! infra:
//!   cloudformation:
//!     stack: shop-network
//!     template: deploy/network.yaml       # CloudFormation's or SAM's, or CDK's synthesized output
//!     parameters: { Env: "${{ env.name }}", ImageTag: "${{ release.version }}" }
//!     capabilities: [CAPABILITY_IAM, CAPABILITY_AUTO_EXPAND]
//!     tags: { team: shop }
//! ```
//!
//! - **Stack**: `spec` is the template as its text (`template`: CloudFormation keeps a template as
//!   it's given, so it compares as text), its `parameters` by key, its `capabilities` and its own
//!   `tags`. An apply makes a change set (the plan: `CREATE` for a stack that isn't there yet,
//!   `UPDATE` otherwise) and executes it once CloudFormation has worked it out; one with no changes
//!   is let go. CloudFormation rolls back an update that fails, and the stack reads Degraded until
//!   the next one lands, or until kiln asks for what it rolled back to (a revert, a rollback).
//! - **StackResource**: each resource of the stack, read only; the stack's `status.resources`
//!   lists them.
//!
//! A parameter CloudFormation hides (`NoEcho`) reads as what kiln last gave it, and a template
//! that differs from kiln's only in line ends and trailing space reads as kiln's.

use serde_json::{Map, Value, json};

use super::{RenderInput, object, status, text};
use crate::health::Status;
use crate::model::Health;

pub const GROUP: &str = "cloudformation.aws.kiln.dev";

/// How long a template may be inline (CloudFormation's limit); a longer one goes through S3.
pub const INLINE_MAX: usize = 51_200;

/// What CloudFormation shows of a parameter it hides.
pub const HIDDEN: &str = "****";

/// The files a render reads.
pub fn files(spec: &Value) -> Vec<String> {
    super::file_settings(spec, &["template"])
}

/// A setting that is a map of text: `parameters`, `tags`.
fn text_map(spec: &Value, key: &str) -> Result<Map<String, Value>, String> {
    match spec.get(key) {
        None | Some(Value::Null) => Ok(Map::new()),
        Some(Value::Object(m)) => m
            .iter()
            .map(|(k, v)| {
                let t = match v {
                    Value::String(s) => s.clone(),
                    Value::Number(_) | Value::Bool(_) => v.to_string(),
                    Value::Array(a) => a.iter().map(|x| x.as_str().map_or_else(|| x.to_string(), str::to_owned)).collect::<Vec<_>>().join(","),
                    _ => return Err(format!("{key}.{k}: text, a number or a list")),
                };
                Ok((k.clone(), Value::String(t)))
            })
            .collect(),
        Some(_) => Err(format!("{key}: a map, by name")),
    }
}

/// An app's stack.
pub fn render(input: &RenderInput<'_>) -> Result<Vec<Value>, String> {
    let stack = text(input.spec, "stack").ok_or("cloudformation needs stack: the stack's name")?;
    if !stack.chars().next().is_some_and(|c| c.is_ascii_alphabetic()) || !stack.chars().all(|c| c.is_ascii_alphanumeric() || c == '-') || stack.len() > 128 {
        return Err(format!("stack `{stack}`: letters, digits and -, starting with a letter, at most 128"));
    }
    let template = match input.spec.get("template") {
        Some(Value::String(path)) => {
            let bytes = (input.read)(path).ok_or_else(|| format!("template: no file {path} in the source"))?;
            String::from_utf8(bytes).map_err(|_| format!("template {path} isn't text"))?
        }
        Some(v @ Value::Object(_)) => serde_json::to_string_pretty(v).map_err(|e| e.to_string())?,
        _ => return Err("cloudformation needs template: a template in the source, or an object".into()),
    };
    if template.trim().is_empty() {
        return Err("the template is empty".into());
    }
    let capabilities: Vec<Value> = match input.spec.get("capabilities") {
        None | Some(Value::Null) => Vec::new(),
        Some(Value::Array(a)) => a.iter().map(|c| c.as_str().map(|s| Value::String(s.to_owned())).ok_or("capabilities: a list of names (CAPABILITY_IAM, …)")).collect::<Result<_, _>>()?,
        Some(_) => return Err("capabilities: a list of names (CAPABILITY_IAM, …)".into()),
    };
    let mut spec = Map::new();
    spec.insert("template".into(), Value::String(template));
    spec.insert("parameters".into(), Value::Object(text_map(input.spec, "parameters")?));
    if !capabilities.is_empty() {
        spec.insert("capabilities".into(), Value::Array(capabilities));
    }
    let tags = text_map(input.spec, "tags")?;
    if !tags.is_empty() {
        spec.insert("tags".into(), Value::Object(tags));
    }
    Ok(vec![object(GROUP, "Stack", stack, None, Value::Object(spec))])
}

/// The template's text as kiln compares it: line ends and trailing space aside.
fn template_text(s: &str) -> String {
    s.replace("\r\n", "\n").lines().map(str::trim_end).collect::<Vec<_>>().join("\n").trim().to_owned()
}

// ------------------------------------------------------------------ CloudFormation's answers → objects

/// A Stack object from one of DescribeStacks' stacks (as JSON, its members arrays), with the
/// template GetTemplate gave (`TemplateStage: Original`), the resources DescribeStackResources
/// listed, and the stack kiln last applied (`applied`).
pub fn live_stack(stack: &Value, template: Option<&str>, resources: &[Value], applied: Option<&Value>) -> Option<Value> {
    let given = applied.and_then(|a| a.pointer("/spec/parameters"));
    let name = stack.get("StackName")?.as_str()?;
    let get = |k: &str| stack.get(k).and_then(Value::as_str).unwrap_or_default().to_owned();
    let mut parameters = Map::new();
    for p in stack.get("Parameters").and_then(Value::as_array).into_iter().flatten() {
        let key = p.get("ParameterKey").and_then(Value::as_str).unwrap_or_default();
        let mut value = p.get("ParameterValue").and_then(Value::as_str).unwrap_or_default().to_owned();
        if value == HIDDEN
            && let Some(v) = given.and_then(|g| g.get(key)).and_then(Value::as_str)
        {
            value = v.to_owned();
        }
        parameters.insert(key.to_owned(), Value::String(value));
    }
    let tags: Map<String, Value> = stack
        .get("Tags")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|t| Some((t.get("Key")?.as_str()?.to_owned(), Value::String(t.get("Value").and_then(Value::as_str).unwrap_or_default().to_owned()))))
        .collect();
    let user: Map<String, Value> = tags.iter().filter(|(k, _)| !k.starts_with("kiln.dev/")).map(|(k, v)| (k.clone(), v.clone())).collect();
    let mut spec = Map::new();
    if let Some(t) = template {
        let kilns = applied.and_then(|a| a.pointer("/spec/template")).and_then(Value::as_str).filter(|k| template_text(k) == template_text(t));
        spec.insert("template".into(), Value::String(kilns.unwrap_or(t).to_owned()));
    }
    spec.insert("parameters".into(), Value::Object(parameters));
    let caps: Vec<Value> = stack.get("Capabilities").and_then(Value::as_array).cloned().unwrap_or_default();
    if !caps.is_empty() {
        spec.insert("capabilities".into(), Value::Array(caps));
    }
    if !user.is_empty() {
        spec.insert("tags".into(), Value::Object(user));
    }
    let mut o = object(GROUP, "Stack", name, None, Value::Object(spec));
    let outputs: Map<String, Value> = stack
        .get("Outputs")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|x| Some((x.get("OutputKey")?.as_str()?.to_owned(), x.get("OutputValue").cloned().unwrap_or(Value::Null))))
        .collect();
    let resources: Vec<Value> = resources
        .iter()
        .map(|r| json!({ "logicalId": r.get("LogicalResourceId"), "type": r.get("ResourceType"), "physicalId": r.get("PhysicalResourceId"), "status": r.get("ResourceStatus"), "reason": r.get("ResourceStatusReason") }))
        .collect();
    if let Value::Object(m) = &mut o {
        m.insert(
            "status".into(),
            json!({ "stackId": get("StackId"), "status": get("StackStatus"), "reason": get("StackStatusReason"), "lastUpdated": get("LastUpdatedTime"), "drift": stack.pointer("/DriftInformation/StackDriftStatus"), "outputs": outputs, "resources": resources }),
        );
    }
    let kiln: Map<String, Value> = tags.into_iter().filter(|(k, _)| k.starts_with("kiln.dev/")).collect();
    super::aws_lambda::tracking_from_tags(&mut o, Some(&Value::Object(kiln)));
    Some(o)
}

/// Whether the stack as it is already is what kiln wants: the template's text, every parameter
/// kiln gives, the capabilities and its own tags.
pub fn stack_matches(desired: &Value, live: &Value) -> bool {
    let d = |k: &str| desired.pointer(&format!("/spec/{k}"));
    let l = |k: &str| live.pointer(&format!("/spec/{k}"));
    let same_template = match (d("template").and_then(Value::as_str), l("template").and_then(Value::as_str)) {
        (Some(a), Some(b)) => template_text(a) == template_text(b),
        _ => false,
    };
    let empty = Value::Object(Map::new());
    let params = |o: Option<&Value>| o.cloned().unwrap_or_else(|| empty.clone());
    // The template's defaults read back too: what kiln gives is what counts.
    same_template && super::aws_lambda::covers(&params(d("parameters")), &params(l("parameters"))) && super::aws_lambda::covers(&params(d("tags")), &params(l("tags"))) && caps(desired) == caps(live)
}

fn caps(o: &Value) -> Vec<String> {
    let mut c: Vec<String> = o.pointer("/spec/capabilities").and_then(Value::as_array).into_iter().flatten().filter_map(Value::as_str).map(str::to_owned).collect();
    c.sort();
    c
}

// ------------------------------------------------------------------ objects → CloudFormation's requests

/// A change set's name for what `desired` asks: the same ask, the same name.
pub fn change_set_name(desired: &Value) -> String {
    format!("kiln-{:016x}", crate::obj::hash_value(desired.get("spec").unwrap_or(&Value::Null)))
}

/// The tags a stack is made and changed with: its own and kiln's.
pub fn tags_of(desired: &Value) -> Vec<(String, String)> {
    let mut all: Map<String, Value> = desired.pointer("/spec/tags").and_then(Value::as_object).cloned().unwrap_or_default();
    for (k, v) in super::aws_lambda::tags_of(&json!({ "metadata": desired.get("metadata").cloned().unwrap_or(Value::Null) })) {
        all.insert(k, v);
    }
    all.into_iter().map(|(k, v)| (k, v.as_str().map_or_else(|| v.to_string(), str::to_owned))).collect()
}

/// CreateChangeSet's params, but the template's (`TemplateBody`, or `TemplateURL` for one in S3):
/// `kind` is `CREATE` or `UPDATE`.
pub fn change_set_params(desired: &Value, kind: &str) -> Vec<(String, String)> {
    let mut out = vec![
        ("StackName".to_owned(), crate::obj::name(desired).to_owned()),
        ("ChangeSetName".to_owned(), change_set_name(desired)),
        ("ChangeSetType".to_owned(), kind.to_owned()),
        ("Description".to_owned(), "kiln".to_owned()),
    ];
    let params: Vec<(String, String)> = desired.pointer("/spec/parameters").and_then(Value::as_object).into_iter().flatten().map(|(k, v)| (k.clone(), v.as_str().unwrap_or_default().to_owned())).collect();
    for (i, (k, v)) in params.iter().enumerate() {
        out.push((format!("Parameters.member.{}.ParameterKey", i + 1), k.clone()));
        out.push((format!("Parameters.member.{}.ParameterValue", i + 1), v.clone()));
    }
    for (i, c) in caps(desired).iter().enumerate() {
        out.push((format!("Capabilities.member.{}", i + 1), c.clone()));
    }
    for (i, (k, v)) in tags_of(desired).iter().enumerate() {
        out.push((format!("Tags.member.{}.Key", i + 1), k.clone()));
        out.push((format!("Tags.member.{}.Value", i + 1), v.clone()));
    }
    out
}

/// Whether a change set failed only because it changes nothing.
pub fn no_changes(reason: &str) -> bool {
    reason.contains("didn't contain changes") || reason.contains("No updates are to be performed")
}

// ------------------------------------------------------------------ health

/// A stack's status: one CloudFormation is still working on, one it can't change (being worked on),
/// and one a failed attempt left.
pub fn busy(status: &str) -> bool {
    status.ends_with("_IN_PROGRESS")
}

/// What kiln last applied to an object, as its last-applied configuration carries it.
fn last_applied(obj: &Value) -> Option<Value> {
    crate::obj::annotation(obj, crate::diff::LAST_APPLIED).and_then(|t| serde_json::from_str(t).ok())
}

pub fn health(obj: &Value) -> Option<Status> {
    match crate::obj::kind(obj) {
        "Stack" => {
            let st = crate::obj::str_at(obj, &["status", "status"]).unwrap_or_default();
            let reason = crate::obj::str_at(obj, &["status", "reason"]).unwrap_or_default();
            let why = |s: &str| if reason.is_empty() { s.to_owned() } else { format!("{s}: {reason}") };
            match st {
                "CREATE_COMPLETE" | "UPDATE_COMPLETE" | "IMPORT_COMPLETE" => status(Health::Healthy, st),
                // Rolled back to what kiln asks for now (a revert of the update that failed): fine.
                "UPDATE_ROLLBACK_COMPLETE" if last_applied(obj).is_some_and(|last| stack_matches(&last, obj)) => status(Health::Healthy, "rolled back to what kiln asks for"),
                "UPDATE_ROLLBACK_COMPLETE" | "IMPORT_ROLLBACK_COMPLETE" => status(Health::Degraded, why("the update failed and CloudFormation rolled it back")),
                "ROLLBACK_COMPLETE" => status(Health::Degraded, why("it failed to create, and CloudFormation rolled it back")),
                "DELETE_COMPLETE" => status(Health::Missing, "deleted"),
                s if s.contains("ROLLBACK") && busy(s) => status(Health::Degraded, why("rolling back")),
                s if s.ends_with("_FAILED") => status(Health::Degraded, why(s)),
                s if busy(s) => status(Health::Progressing, s.to_ascii_lowercase().replace('_', " ")),
                "" => None,
                other => status(Health::Progressing, other),
            }
        }
        "StackResource" => match crate::obj::str_at(obj, &["status", "status"]) {
            Some(s) if s.ends_with("_COMPLETE") && !s.contains("ROLLBACK") => status(Health::Healthy, ""),
            Some(s) if s.ends_with("_FAILED") => status(Health::Degraded, s),
            Some(s) => status(Health::Progressing, s),
            None => None,
        },
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const TEMPLATE: &str = "AWSTemplateFormatVersion: '2010-09-09'\nParameters:\n  Env: { Type: String }\nResources:\n  Bucket:\n    Type: AWS::S3::Bucket\n    Properties:\n      BucketName: !Sub 'shop-${Env}'\n";

    fn render_net(extra: Value) -> Result<Vec<Value>, String> {
        let mut spec = json!({ "stack": "shop-network", "template": "deploy/net.yaml", "parameters": { "Env": "staging", "Count": 2, "Zones": ["a", "b"] }, "capabilities": ["CAPABILITY_IAM"], "tags": { "team": "shop" } });
        if let (Value::Object(m), Value::Object(e)) = (&mut spec, extra) {
            m.extend(e);
        }
        let read = |p: &str| (p == "deploy/net.yaml").then(|| TEMPLATE.as_bytes().to_vec());
        let none = Map::new();
        render(&RenderInput { spec: &spec, target: &none, app: "infra", namespace: "", revision: "abc", read: &read })
    }

    #[test]
    fn a_definition_renders_its_stack_as_a_change_set_asks_for_it() -> Result<(), String> {
        let objs = render_net(json!({}))?;
        let stack = objs.first().ok_or("no stack")?;
        assert_eq!(crate::obj::name(stack), "shop-network");
        assert_eq!(stack.pointer("/spec/template"), Some(&json!(TEMPLATE)), "the template as its text: !Sub kept");
        assert_eq!(stack.pointer("/spec/parameters"), Some(&json!({ "Env": "staging", "Count": "2", "Zones": "a,b" })), "parameters as CloudFormation takes them");
        let params = change_set_params(stack, "CREATE");
        let get = |k: &str| params.iter().find(|(p, _)| p == k).map(|(_, v)| v.as_str());
        assert_eq!(get("ChangeSetType"), Some("CREATE"));
        assert_eq!(get("Capabilities.member.1"), Some("CAPABILITY_IAM"));
        assert!(params.iter().any(|(k, v)| k.ends_with("ParameterKey") && v == "Env"));
        assert!(get("ChangeSetName").is_some_and(|n| n.starts_with("kiln-")), "{params:?}");
        assert_eq!(change_set_name(stack), change_set_name(&render_net(json!({}))?.into_iter().next().unwrap_or_default()), "the same ask, the same change set");
        assert!(render_net(json!({ "stack": "1st" })).is_err(), "a stack's name starts with a letter");
        assert!(render_net(json!({ "template": "nope.yaml" })).is_err_and(|e| e.contains("no file nope.yaml")));
        let inline = render_net(json!({ "template": { "Resources": { "Topic": { "Type": "AWS::SNS::Topic" } } } }))?;
        assert!(inline.first().and_then(|s| s.pointer("/spec/template")).and_then(Value::as_str).is_some_and(|t| t.contains("AWS::SNS::Topic")));
        Ok(())
    }

    #[test]
    fn stacks_read_back_and_health_follows_their_status() -> Result<(), String> {
        let desired = render_net(json!({ "parameters": { "Env": "staging", "Secret": "hunter2" } }))?.into_iter().next().ok_or("stack")?;
        let answer = json!({
            "StackName": "shop-network", "StackId": "arn:aws:cloudformation:us-east-1:1:stack/shop-network/x", "StackStatus": "UPDATE_COMPLETE",
            "Parameters": [{ "ParameterKey": "Env", "ParameterValue": "staging" }, { "ParameterKey": "Secret", "ParameterValue": HIDDEN }],
            "Capabilities": ["CAPABILITY_IAM"], "Tags": [{ "Key": "team", "Value": "shop" }, { "Key": "kiln.dev/app", "Value": "00ff" }],
            "Outputs": [{ "OutputKey": "BucketName", "OutputValue": "shop-staging" }]
        });
        let live = live_stack(&answer, Some(&format!("{TEMPLATE}\n\n")), &[json!({ "LogicalResourceId": "Bucket", "ResourceType": "AWS::S3::Bucket", "ResourceStatus": "UPDATE_COMPLETE" })], Some(&desired)).ok_or("no live stack")?;
        assert!(stack_matches(&desired, &live), "a hidden parameter reads as kiln gave it: {live}");
        assert_eq!(live.pointer("/spec/template"), desired.pointer("/spec/template"), "trailing lines aside, the template reads as kiln's");
        assert_eq!(crate::obj::label(&live, crate::engine::APP_LABEL), Some("00ff"));
        assert_eq!(live.pointer("/spec/tags"), Some(&json!({ "team": "shop" })));
        assert_eq!(live.pointer("/status/outputs/BucketName"), Some(&json!("shop-staging")));
        assert_eq!(live.pointer("/status/resources/0/logicalId"), Some(&json!("Bucket")));
        let mut changed = desired.clone();
        crate::providers::set_path(&mut changed, "spec.parameters.Env", json!("production"))?;
        assert!(!stack_matches(&changed, &live));
        let with = |s: &str, reason: &str| {
            let mut o = live.clone();
            crate::providers::set_path(&mut o, "status.status", json!(s)).ok();
            crate::providers::set_path(&mut o, "status.reason", json!(reason)).ok();
            health(&o).map(|h| (h.health, h.message))
        };
        assert_eq!(with("UPDATE_COMPLETE", "").map(|h| h.0), Some(Health::Healthy));
        assert_eq!(with("UPDATE_IN_PROGRESS", "").map(|h| h.0), Some(Health::Progressing));
        assert_eq!(with("UPDATE_ROLLBACK_IN_PROGRESS", "Bucket exists").map(|h| h.0), Some(Health::Degraded));
        assert_eq!(with("UPDATE_ROLLBACK_COMPLETE", "Bucket exists"), Some((Health::Degraded, "the update failed and CloudFormation rolled it back: Bucket exists".to_owned())));
        assert_eq!(with("ROLLBACK_COMPLETE", "").map(|h| h.0), Some(Health::Degraded));
        assert!(no_changes("The submitted information didn't contain changes. Submit different information to create a change set."));
        Ok(())
    }
}
