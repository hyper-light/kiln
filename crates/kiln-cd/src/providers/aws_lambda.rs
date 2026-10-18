//! `aws-lambda` (docs/cd-targets.md §3): a function, the alias kiln moves to each version it
//! publishes, and optionally the alias's function URL.
//!
//! ```yaml
//! thumbnails:
//!   aws-lambda:
//!     function: thumbnails
//!     alias: live                         # the default
//!     code: "${{ artifacts.zip }}"        # s3://bucket/key, an image, or a zip CI built
//!     config: deploy/thumbnails.json      # CreateFunction's input: Runtime, Handler, Role, …
//!     set: { "Environment.Variables.MODE": prod }
//!     url: { AuthType: NONE }
//! ```
//!
//! - **Function** (`thumbnails`): `spec` is CreateFunction's input as Lambda spells it, with
//!   `Code` as one of `{S3Bucket, S3Key}`, `{ImageUri}` or `{Artifact: sha256:…}` (a zip kiln
//!   uploads to its bucket in the account). Lambda doesn't report where code came from, so the
//!   server reads back what it last applied while the function's `CodeSha256` is still what that
//!   apply made; another hash is drift.
//! - **Alias** (`thumbnails:live`): `FunctionVersion: $PUBLISHED`, the newest version, which is
//!   the one kiln published with the function's last change. An alias on any other version reads
//!   as that version: drift, or a rollout in progress.
//! - **FunctionUrl** (`thumbnails:live`): the alias's URL.
//!
//! Waves keep the order: the function (and its new version), then the alias, then the URL.

use serde_json::{Map, Value, json};

use super::{RenderInput, apply_sets, file_or_object, file_settings, object, status, text};
use crate::health::Status;
use crate::model::Health;

pub const GROUP: &str = "lambda.aws.kiln.dev";

/// What an alias points at to follow the function: its newest published version.
pub const PUBLISHED: &str = "$PUBLISHED";

/// Configuration fields: CreateFunction's and UpdateFunctionConfiguration's.
pub const CONFIG_FIELDS: &[&str] = &[
    "Role",
    "Handler",
    "Description",
    "Timeout",
    "MemorySize",
    "VpcConfig",
    "Environment",
    "Runtime",
    "DeadLetterConfig",
    "KMSKeyArn",
    "TracingConfig",
    "Layers",
    "FileSystemConfigs",
    "ImageConfig",
    "EphemeralStorage",
    "SnapStart",
    "LoggingConfig",
];

/// The files a render reads.
pub fn files(spec: &Value) -> Vec<String> {
    file_settings(spec, &["config"])
}

/// Where code comes from, as `code:` says it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CodeSource {
    S3 { bucket: String, key: String, version: Option<String> },
    Image(String),
    /// A zip kiln holds (a CI artifact), by its SHA-256 (hex).
    Artifact(String),
}

/// Reads `code:`: `s3://bucket/key[?versionId=…]`, `sha256:<hex>` (a zip CI built), or an image.
pub fn code_source(code: &str) -> Result<CodeSource, String> {
    let code = code.trim();
    if let Some(rest) = code.strip_prefix("s3://") {
        let (path, version) = match rest.split_once("?versionId=") {
            Some((p, v)) => (p, Some(v.to_owned())),
            None => (rest, None),
        };
        let (bucket, key) = path.split_once('/').filter(|(b, k)| !b.is_empty() && !k.is_empty()).ok_or_else(|| format!("`{code}`: s3://bucket/key"))?;
        return Ok(CodeSource::S3 { bucket: bucket.to_owned(), key: key.to_owned(), version });
    }
    if let Some(hex) = code.strip_prefix("sha256:").filter(|h| h.len() == 64 && h.bytes().all(|b| b.is_ascii_hexdigit())) {
        return Ok(CodeSource::Artifact(hex.to_ascii_lowercase()));
    }
    if code.contains('/') && !code.contains("://") && !code.contains(' ') {
        return Ok(CodeSource::Image(code.to_owned()));
    }
    Err(format!("`{code}` is no code Lambda takes: s3://bucket/key, an image, or a zip CI built (${{{{ artifacts.<name> }}}})"))
}

impl CodeSource {
    /// As `spec.Code`.
    pub fn spec(&self) -> Value {
        match self {
            CodeSource::S3 { bucket, key, version } => {
                let mut m = json!({ "S3Bucket": bucket, "S3Key": key });
                if let (Some(v), Value::Object(o)) = (version, &mut m) {
                    o.insert("S3ObjectVersion".into(), Value::String(v.clone()));
                }
                m
            }
            CodeSource::Image(uri) => json!({ "ImageUri": uri }),
            CodeSource::Artifact(hex) => json!({ "Artifact": format!("sha256:{hex}") }),
        }
    }

    pub fn package_type(&self) -> &'static str {
        if matches!(self, CodeSource::Image(_)) { "Image" } else { "Zip" }
    }
}

/// An app's objects.
pub fn render(input: &RenderInput<'_>) -> Result<Vec<Value>, String> {
    let function = text(input.spec, "function").ok_or("aws-lambda needs function: the function's name")?;
    let alias = text(input.spec, "alias").unwrap_or("live");
    let code = code_source(text(input.spec, "code").ok_or("aws-lambda needs code: s3://bucket/key, an image, or ${{ artifacts.<name> }}")?)?;
    let mut spec = match file_or_object(input, "config")? {
        Some(Value::Object(m)) => Value::Object(m),
        Some(_) => return Err("config: an object (CreateFunction's input)".into()),
        None => Value::Object(Map::new()),
    };
    if let Value::Object(m) = &mut spec {
        // What the definition says about the function itself wins over the file.
        m.remove("FunctionName");
        m.remove("Code");
        m.remove("Publish");
        m.insert("Code".into(), code.spec());
        m.insert("PackageType".into(), Value::String(code.package_type().into()));
    }
    apply_sets(&mut spec, input.spec.get("set"))?;
    if code.package_type() == "Zip" && (spec.get("Runtime").is_none() || spec.get("Handler").is_none()) {
        return Err(format!("function {function}: a zip needs Runtime and Handler (in config or set)"));
    }
    if spec.get("Role").is_none() {
        return Err(format!("function {function}: needs Role, the execution role's ARN (in config or set)"));
    }
    let mut out = vec![wave(object(GROUP, "Function", function, None, spec), 0)];
    let alias_name = format!("{function}:{alias}");
    out.push(wave(object(GROUP, "Alias", &alias_name, None, json!({ "FunctionName": function, "Name": alias, "FunctionVersion": PUBLISHED })), 1));
    if let Some(url) = input.spec.get("url").filter(|u| !u.is_null()) {
        let Value::Object(u) = url else { return Err("url: an object ({AuthType: NONE | AWS_IAM, Cors})".into()) };
        let mut s = u.clone();
        s.insert("FunctionName".into(), Value::String(function.to_owned()));
        s.insert("Qualifier".into(), Value::String(alias.to_owned()));
        s.entry("AuthType").or_insert(Value::String("AWS_IAM".into()));
        out.push(wave(object(GROUP, "FunctionUrl", &alias_name, None, Value::Object(s)), 2));
    }
    Ok(out)
}

fn wave(mut o: Value, w: i64) -> Value {
    if let Some(ann) = crate::obj::ensure_map(&mut o, &["metadata", "annotations"]) {
        ann.insert(crate::sync::KILN_WAVE.into(), Value::String(w.to_string()));
    }
    o
}

// ------------------------------------------------------------------ Lambda's answers → objects

/// A Function object from GetFunction's answer. `applied` is the `spec.Code` kiln last applied
/// and the `CodeSha256` it made, so code reads as what kiln deployed while it still is.
pub fn live_function(answer: &Value, applied: Option<(&Value, &str)>) -> Option<Value> {
    let cfg = answer.get("Configuration")?;
    let name = cfg.get("FunctionName")?.as_str()?;
    let mut spec = Map::new();
    for f in CONFIG_FIELDS {
        if let Some(v) = cfg.get(*f).filter(|v| !v.is_null()) {
            spec.insert((*f).to_owned(), v.clone());
        }
    }
    // Lambda answers with layers as objects and configuration in wrappers.
    if let Some(Value::Array(layers)) = spec.get("Layers").cloned() {
        spec.insert("Layers".into(), Value::Array(layers.iter().filter_map(|l| l.get("Arn").cloned().or_else(|| l.as_str().map(|s| Value::String(s.to_owned())))).collect()));
    }
    if let Some(ic) = cfg.pointer("/ImageConfigResponse/ImageConfig") {
        spec.insert("ImageConfig".into(), ic.clone());
    }
    if let Some(vpc) = spec.get_mut("VpcConfig").and_then(Value::as_object_mut) {
        vpc.remove("VpcId");
        if vpc.get("SubnetIds").and_then(Value::as_array).is_some_and(Vec::is_empty) && vpc.get("SecurityGroupIds").and_then(Value::as_array).is_some_and(Vec::is_empty) {
            spec.remove("VpcConfig");
        }
    }
    if let Some(a) = cfg.get("Architectures") {
        spec.insert("Architectures".into(), a.clone());
    }
    let package = cfg.get("PackageType").and_then(Value::as_str).unwrap_or("Zip");
    spec.insert("PackageType".into(), Value::String(package.to_owned()));
    let sha = cfg.get("CodeSha256").and_then(Value::as_str).unwrap_or_default();
    let code = match applied {
        Some((code, made)) if made == sha => code.clone(),
        _ if package == "Image" => json!({ "ImageUri": answer.pointer("/Code/ImageUri").and_then(Value::as_str).unwrap_or_default() }),
        _ => json!({ "CodeSha256": sha }),
    };
    spec.insert("Code".into(), code);
    if let Some(Value::Object(tags)) = answer.get("Tags") {
        let user: Map<String, Value> = tags.iter().filter(|(k, _)| !k.starts_with("kiln.dev/")).map(|(k, v)| (k.clone(), v.clone())).collect();
        if !user.is_empty() {
            spec.insert("Tags".into(), Value::Object(user));
        }
    }
    let mut o = object(GROUP, "Function", name, None, Value::Object(spec));
    let st: Map<String, Value> = ["State", "StateReason", "StateReasonCode", "LastUpdateStatus", "LastUpdateStatusReason", "LastUpdateStatusReasonCode", "CodeSha256", "CodeSize", "LastModified", "FunctionArn", "RevisionId", "Version"]
        .iter()
        .filter_map(|k| cfg.get(*k).filter(|v| !v.is_null()).map(|v| ((*k).to_owned(), v.clone())))
        .collect();
    if let Value::Object(m) = &mut o {
        m.insert("status".into(), Value::Object(st));
    }
    tracking_from_tags(&mut o, answer.get("Tags"));
    Some(o)
}

/// An Alias object from GetAlias's answer; `newest` is the function's newest published version.
pub fn live_alias(function: &str, answer: &Value, newest: Option<&str>) -> Option<Value> {
    let name = answer.get("Name")?.as_str()?;
    let version = answer.get("FunctionVersion").and_then(Value::as_str).unwrap_or_default();
    let shown = if newest == Some(version) { PUBLISHED } else { version };
    let mut spec = json!({ "FunctionName": function, "Name": name, "FunctionVersion": shown });
    if let (Some(d), Value::Object(m)) = (answer.get("Description").and_then(Value::as_str).filter(|d| !d.is_empty()), &mut spec) {
        m.insert("Description".into(), Value::String(d.to_owned()));
    }
    if let (Some(r), Value::Object(m)) = (answer.get("RoutingConfig").filter(|r| r.get("AdditionalVersionWeights").and_then(Value::as_object).is_some_and(|w| !w.is_empty())), &mut spec) {
        m.insert("RoutingConfig".into(), r.clone());
    }
    let mut o = object(GROUP, "Alias", &format!("{function}:{name}"), None, spec);
    if let Value::Object(m) = &mut o {
        m.insert("status".into(), json!({ "AliasArn": answer.get("AliasArn"), "FunctionVersion": version, "RevisionId": answer.get("RevisionId") }));
    }
    Some(o)
}

/// A FunctionUrl object from GetFunctionUrlConfig's answer.
pub fn live_url(function: &str, qualifier: &str, answer: &Value) -> Option<Value> {
    let mut spec = Map::new();
    spec.insert("FunctionName".into(), Value::String(function.to_owned()));
    spec.insert("Qualifier".into(), Value::String(qualifier.to_owned()));
    for k in ["AuthType", "Cors", "InvokeMode"] {
        if let Some(v) = answer.get(k).filter(|v| !v.is_null()) {
            spec.insert(k.to_owned(), v.clone());
        }
    }
    let mut o = object(GROUP, "FunctionUrl", &format!("{function}:{qualifier}"), None, Value::Object(spec));
    if let Value::Object(m) = &mut o {
        m.insert("status".into(), json!({ "FunctionUrl": answer.get("FunctionUrl"), "FunctionArn": answer.get("FunctionArn") }));
    }
    Some(o)
}

/// A Version object (a child of its function, for the tree).
pub fn live_version(function: &str, cfg: &Value) -> Option<Value> {
    let v = cfg.get("Version")?.as_str()?;
    let mut o = object(GROUP, "Version", &format!("{function}:{v}"), None, json!({ "FunctionName": function, "CodeSha256": cfg.get("CodeSha256"), "Description": cfg.get("Description") }));
    if let Value::Object(m) = &mut o {
        m.insert("status".into(), json!({ "FunctionArn": cfg.get("FunctionArn"), "LastModified": cfg.get("LastModified"), "State": cfg.get("State") }));
        if let Some(meta) = m.get_mut("metadata").and_then(Value::as_object_mut) {
            meta.insert("ownerReferences".into(), json!([{ "apiVersion": format!("{GROUP}/v1"), "kind": "Function", "name": function }]));
        }
    }
    Some(o)
}

/// Copies kiln's tracking tags into the object's label and annotation.
pub fn tracking_from_tags(o: &mut Value, tags: Option<&Value>) {
    let Some(Value::Object(tags)) = tags else { return };
    if let Some(v) = tags.get(crate::engine::APP_LABEL).and_then(Value::as_str)
        && let Some(l) = crate::obj::ensure_map(o, &["metadata", "labels"])
    {
        l.insert(crate::engine::APP_LABEL.into(), Value::String(v.to_owned()));
    }
    if let Some(v) = tags.get(crate::engine::TRACKING).and_then(Value::as_str)
        && let Some(a) = crate::obj::ensure_map(o, &["metadata", "annotations"])
    {
        a.insert(crate::engine::TRACKING.into(), Value::String(v.to_owned()));
    }
}

/// The tags an object is applied with: its own (`spec.Tags`), and kiln's tracking.
pub fn tags_of(desired: &Value) -> Map<String, Value> {
    let mut tags: Map<String, Value> = desired.pointer("/spec/Tags").and_then(Value::as_object).cloned().unwrap_or_default();
    if let Some(v) = crate::obj::label(desired, crate::engine::APP_LABEL) {
        tags.insert(crate::engine::APP_LABEL.into(), Value::String(v.to_owned()));
    }
    if let Some(v) = crate::obj::annotation(desired, crate::engine::TRACKING) {
        // AWS tag values hold 256 characters.
        tags.insert(crate::engine::TRACKING.into(), Value::String(v.chars().take(256).collect()));
    }
    tags
}

// ------------------------------------------------------------------ objects → Lambda's requests

/// CreateFunction's body, for code at `code` (the artifact's place in S3, for an artifact).
pub fn create_body(desired: &Value, code: &Value) -> Value {
    let spec = desired.get("spec").cloned().unwrap_or(Value::Null);
    let mut b = Map::new();
    b.insert("FunctionName".into(), Value::String(crate::obj::name(desired).to_owned()));
    for f in CONFIG_FIELDS.iter().chain(&["PackageType", "Architectures", "CodeSigningConfigArn"]) {
        if let Some(v) = spec.get(*f) {
            b.insert((*f).to_owned(), v.clone());
        }
    }
    b.insert("Code".into(), code.clone());
    b.insert("Tags".into(), Value::Object(tags_of(desired)));
    b.insert("Publish".into(), Value::Bool(false));
    Value::Object(b)
}

/// UpdateFunctionConfiguration's body: the configuration fields kiln sets.
pub fn configuration_body(desired: &Value) -> Value {
    let spec = desired.get("spec").cloned().unwrap_or(Value::Null);
    Value::Object(CONFIG_FIELDS.iter().filter_map(|f| spec.get(*f).map(|v| ((*f).to_owned(), v.clone()))).collect())
}

/// UpdateFunctionCode's body, for code at `code`.
pub fn code_body(desired: &Value, code: &Value) -> Value {
    let mut b = code.as_object().cloned().unwrap_or_default();
    if let Some(a) = desired.pointer("/spec/Architectures") {
        b.insert("Architectures".into(), a.clone());
    }
    Value::Object(b)
}

/// Whether the live function's configuration already has every field kiln sets.
pub fn configuration_matches(desired: &Value, live: &Value) -> bool {
    CONFIG_FIELDS.iter().all(|f| match desired.pointer(&format!("/spec/{f}")) {
        None => true,
        Some(d) => live.pointer(&format!("/spec/{f}")).is_some_and(|l| covers(d, l)),
    })
}

/// Whether the live function runs the code kiln wants.
pub fn code_matches(desired: &Value, live: &Value) -> bool {
    let (d, l) = (desired.pointer("/spec/Code"), live.pointer("/spec/Code"));
    let arch = match desired.pointer("/spec/Architectures") {
        None => true,
        Some(a) => live.pointer("/spec/Architectures").is_some_and(|l| covers(a, l)),
    };
    arch && matches!((d, l), (Some(d), Some(l)) if covers(d, l))
}

/// Whether every field of `want` is in `have` with its value (lists item by item; numbers by
/// value).
pub fn covers(want: &Value, have: &Value) -> bool {
    match (want, have) {
        (Value::Object(w), Value::Object(h)) => w.iter().all(|(k, v)| match h.get(k) {
            Some(hv) => covers(v, hv),
            None => v.is_null() || v.as_object().is_some_and(Map::is_empty) || v.as_array().is_some_and(Vec::is_empty),
        }),
        (Value::Array(w), Value::Array(h)) => w.len() == h.len() && w.iter().zip(h).all(|(a, b)| covers(a, b)),
        (Value::Number(a), Value::Number(b)) => a.as_f64() == b.as_f64(),
        (Value::Number(a), Value::String(b)) | (Value::String(b), Value::Number(a)) => b.parse::<f64>().ok() == a.as_f64(),
        (a, b) => a == b,
    }
}

// ------------------------------------------------------------------ health

pub fn health(obj: &Value) -> Option<Status> {
    match crate::obj::kind(obj) {
        "Function" => {
            let st = |k: &str| crate::obj::str_at(obj, &["status", k]).unwrap_or_default();
            match (st("State"), st("LastUpdateStatus")) {
                ("Failed", _) => status(Health::Degraded, st("StateReason")),
                (_, "Failed") => status(Health::Degraded, st("LastUpdateStatusReason")),
                ("Pending", _) | (_, "InProgress") => status(Health::Progressing, "updating"),
                ("Inactive", _) => status(Health::Healthy, "inactive until its next invocation"),
                _ => status(Health::Healthy, ""),
            }
        }
        "Alias" => match obj.pointer("/spec/RoutingConfig/AdditionalVersionWeights").and_then(Value::as_object).filter(|w| !w.is_empty()) {
            Some(w) => status(Health::Progressing, format!("shifting traffic: {}", w.iter().map(|(v, p)| format!("{v} at {p}")).collect::<Vec<_>>().join(", "))),
            None => status(Health::Healthy, ""),
        },
        "FunctionUrl" | "Version" => status(Health::Healthy, ""),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn input<'a>(spec: &'a Value, read: &'a dyn Fn(&str) -> Option<Vec<u8>>, target: &'a Map<String, Value>) -> RenderInput<'a> {
        RenderInput { spec, target, app: "thumbs", namespace: "", revision: "abc", read }
    }

    #[test]
    fn a_definition_renders_function_alias_and_url() -> Result<(), String> {
        let spec = json!({ "function": "thumbs", "code": "s3://bucket/thumbs/abc.zip", "config": "fn.json", "set": { "Environment.Variables.MODE": "prod" }, "url": { "AuthType": "NONE" } });
        let read = |p: &str| (p == "fn.json").then(|| br#"{"Runtime":"python3.13","Handler":"app.handler","Role":"arn:aws:iam::1:role/thumbs","MemorySize":512,"FunctionName":"ignored"}"#.to_vec());
        let none = Map::new();
        let objs = render(&input(&spec, &read, &none))?;
        assert_eq!(objs.len(), 3);
        let f = objs.first().ok_or("no function")?;
        assert_eq!(crate::obj::name(f), "thumbs");
        assert_eq!(f.pointer("/spec/Code"), Some(&json!({ "S3Bucket": "bucket", "S3Key": "thumbs/abc.zip" })));
        assert_eq!(f.pointer("/spec/Environment/Variables/MODE"), Some(&json!("prod")));
        assert_eq!(f.pointer("/spec/FunctionName"), None);
        assert_eq!(crate::obj::annotation(f, crate::sync::KILN_WAVE), Some("0"));
        let a = objs.get(1).ok_or("no alias")?;
        assert_eq!((crate::obj::name(a), a.pointer("/spec/FunctionVersion")), ("thumbs:live", Some(&json!(PUBLISHED))));
        let u = objs.get(2).ok_or("no url")?;
        assert_eq!(u.pointer("/spec/Qualifier"), Some(&json!("live")));
        assert_eq!(files(&spec), vec!["fn.json".to_owned()]);
        // A zip without a runtime, or a function without a role, is refused at render.
        let bare = json!({ "function": "x", "code": "s3://b/k.zip", "set": { "Role": "r" } });
        assert!(render(&input(&bare, &read, &none)).is_err_and(|e| e.contains("Runtime")));
        let image = json!({ "function": "x", "code": "123.dkr.ecr.us-east-1.amazonaws.com/x@sha256:abcd", "set": { "Role": "r" } });
        let objs = render(&input(&image, &read, &none))?;
        assert_eq!(objs.first().and_then(|f| f.pointer("/spec/PackageType")), Some(&json!("Image")));
        Ok(())
    }

    #[test]
    fn code_sources_read() {
        assert_eq!(code_source("s3://b/k/x.zip?versionId=3"), Ok(CodeSource::S3 { bucket: "b".into(), key: "k/x.zip".into(), version: Some("3".into()) }));
        let hex = "a".repeat(64);
        assert_eq!(code_source(&format!("sha256:{hex}")), Ok(CodeSource::Artifact(hex)));
        assert!(matches!(code_source("ghcr.io/acme/fn:1"), Ok(CodeSource::Image(_))));
        assert!(code_source("s3://bucket").is_err());
        assert!(code_source("nonsense").is_err());
    }

    #[test]
    fn lambdas_answers_read_as_objects() -> Result<(), String> {
        let answer = json!({
            "Configuration": {
                "FunctionName": "thumbs", "Runtime": "python3.13", "Handler": "app.handler", "Role": "arn:aws:iam::1:role/thumbs",
                "MemorySize": 512, "Timeout": 3, "CodeSha256": "made=", "State": "Active", "LastUpdateStatus": "Successful",
                "PackageType": "Zip", "Architectures": ["x86_64"], "Layers": [{ "Arn": "arn:layer:1", "CodeSize": 10 }],
                "VpcConfig": { "SubnetIds": [], "SecurityGroupIds": [], "VpcId": "" }
            },
            "Code": { "RepositoryType": "S3", "Location": "https://presigned" },
            "Tags": { "team": "media", "kiln.dev/app": "00ff", "kiln.dev/tracking": "default/thumbs:lambda.aws.kiln.dev/Function:/thumbs" }
        });
        let applied = json!({ "S3Bucket": "bucket", "S3Key": "thumbs/abc.zip" });
        let live = live_function(&answer, Some((&applied, "made="))).ok_or("no object")?;
        assert_eq!(live.pointer("/spec/Code"), Some(&applied));
        assert_eq!(live.pointer("/spec/Layers"), Some(&json!(["arn:layer:1"])));
        assert_eq!(live.pointer("/spec/VpcConfig"), None);
        assert_eq!(live.pointer("/spec/Tags"), Some(&json!({ "team": "media" })));
        assert_eq!(crate::obj::label(&live, crate::engine::APP_LABEL), Some("00ff"));
        assert_eq!(health(&live).map(|s| s.health), Some(Health::Healthy));
        // Code another hash made reads as that hash: drift.
        let drifted = live_function(&answer, Some((&applied, "other="))).ok_or("no object")?;
        assert_eq!(drifted.pointer("/spec/Code"), Some(&json!({ "CodeSha256": "made=" })));
        // What kiln wants against what runs.
        let objs = render(&input(
            &json!({ "function": "thumbs", "code": "s3://bucket/thumbs/abc.zip", "set": { "Runtime": "python3.13", "Handler": "app.handler", "Role": "arn:aws:iam::1:role/thumbs", "MemorySize": 512 } }),
            &|_| None,
            &Map::new(),
        ))?;
        let desired = objs.first().ok_or("no function")?;
        assert!(configuration_matches(desired, &live));
        assert!(code_matches(desired, &live));
        assert!(!code_matches(desired, &drifted));
        let mut bigger = desired.clone();
        crate::providers::set_path(&mut bigger, "spec.MemorySize", json!(1024))?;
        assert!(!configuration_matches(&bigger, &live));
        assert_eq!(configuration_body(&bigger).get("MemorySize"), Some(&json!(1024)));
        let created = create_body(desired, &applied);
        assert_eq!((created.get("Publish"), created.pointer("/Code/S3Key")), (Some(&json!(false)), Some(&json!("thumbs/abc.zip"))));
        Ok(())
    }

    #[test]
    fn aliases_follow_the_newest_version() -> Result<(), String> {
        let answer = json!({ "Name": "live", "FunctionVersion": "7", "AliasArn": "arn:alias" });
        let newest = live_alias("thumbs", &answer, Some("7")).ok_or("no alias")?;
        assert_eq!(newest.pointer("/spec/FunctionVersion"), Some(&json!(PUBLISHED)));
        let older = live_alias("thumbs", &answer, Some("8")).ok_or("no alias")?;
        assert_eq!(older.pointer("/spec/FunctionVersion"), Some(&json!("7")));
        let canary = live_alias("thumbs", &json!({ "Name": "live", "FunctionVersion": "7", "RoutingConfig": { "AdditionalVersionWeights": { "8": 0.1 } } }), Some("8")).ok_or("no alias")?;
        assert_eq!(health(&canary).map(|s| s.health), Some(Health::Progressing));
        Ok(())
    }

    #[test]
    fn health_reads_state_and_updates() {
        let f = |state: &str, update: &str| json!({ "apiVersion": "lambda.aws.kiln.dev/v1", "kind": "Function", "status": { "State": state, "LastUpdateStatus": update, "StateReason": "boom", "LastUpdateStatusReason": "bad zip" } });
        assert_eq!(health(&f("Active", "Successful")).map(|s| s.health), Some(Health::Healthy));
        assert_eq!(health(&f("Active", "InProgress")).map(|s| s.health), Some(Health::Progressing));
        assert_eq!(health(&f("Active", "Failed")).map(|s| (s.health, s.message)), Some((Health::Degraded, "bad zip".to_owned())));
        assert_eq!(health(&f("Failed", "Successful")).map(|s| s.health), Some(Health::Degraded));
    }
}
