//! Attestations (docs/ci.md W18-R01, R02): in-toto statements of SLSA v1.0 provenance and of
//! SBOMs, DSSE envelopes and Sigstore bundles. Pure: the server passes what it knows of the run
//! and the signature it made; nothing here comes from a job's own steps but the subjects.

use std::collections::BTreeMap;

use base64::Engine as _;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::wf::ir::{Step, Tpl, YVal};

pub const STATEMENT: &str = "https://in-toto.io/Statement/v1";
pub const PROVENANCE: &str = "https://slsa.dev/provenance/v1";
pub const SPDX: &str = "https://spdx.dev/Document/v2.3";
pub const CYCLONEDX: &str = "https://cyclonedx.org/bom";
pub const BUILD_TYPE: &str = "https://kiln.dev/ci/workflow@v1";
/// DSSE's payload type for in-toto statements.
pub const PAYLOAD_TYPE: &str = "application/vnd.in-toto+json";
pub const BUNDLE: &str = "application/vnd.dev.sigstore.bundle.v0.3+json";

/// What a statement is about: a file or an image, by its digests (`sha256` → hex).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Subject {
    pub name: String,
    pub digest: BTreeMap<String, String>,
}

/// An in-toto statement of `predicate` (of `predicate_type`) about `subjects`.
pub fn statement(subjects: &[Subject], predicate_type: &str, predicate: Value) -> Value {
    json!({ "_type": STATEMENT, "subject": subjects, "predicateType": predicate_type, "predicate": predicate })
}

/// What the server knows of the build, for its provenance.
#[derive(Debug, Clone, Default)]
pub struct Facts {
    /// `<public_url>/ci@v1`.
    pub builder_id: String,
    /// The repository as kiln serves it (`https://kiln.example/acme/app`).
    pub repository: String,
    pub workflow_path: String,
    /// `refs/heads/main`.
    pub git_ref: String,
    pub sha: String,
    /// `workflow_dispatch` inputs and the like.
    pub inputs: Value,
    pub event: String,
    /// The actions it ran, as pinned when the run started: (`uses`, `owner/repo`, commit).
    pub actions: Vec<(String, String, String)>,
    /// Container images it ran in: (reference, `sha256:…` digest).
    pub images: Vec<(String, String)>,
    /// `<public_url>/runs/<id>`.
    pub run_url: String,
    pub attempt: u32,
    pub runner: String,
    pub executor: String,
    /// How the leg was isolated from its runner (`host`, `container`, `microvm`).
    pub isolation: String,
    /// RFC 3339.
    pub started: Option<String>,
    pub finished: Option<String>,
}

/// The predicate type an SBOM is attested as, by the document's shape: SPDX's or CycloneDX's.
pub fn sbom_type(doc: &Value) -> Option<&'static str> {
    if doc.get("spdxVersion").and_then(Value::as_str).is_some_and(|v| v.starts_with("SPDX-")) {
        Some(SPDX)
    } else if doc.get("bomFormat").and_then(Value::as_str) == Some("CycloneDX") {
        Some(CYCLONEDX)
    } else {
        None
    }
}

/// A job's `attest:` (kiln dialect) as the step it runs after the job's own:
/// `kiln/attest@v1` with its `subjects` (paths or image refs), `provenance` (true unless
/// `false`) and `sbom`. `attest:` may also be the subjects alone, a list or one string.
pub fn job_step(v: &YVal) -> Option<Step> {
    let (subjects, provenance, sbom) = match v {
        YVal::Map(_) => (v.get("subjects").cloned().unwrap_or(YVal::Null), v.get("provenance").cloned(), v.get("sbom").cloned()),
        YVal::Seq(_) | YVal::Str(_) => (v.clone(), None, None),
        YVal::Bool(false) | YVal::Null => return None,
        _ => (YVal::Null, None, None),
    };
    let lines: Vec<String> = match &subjects {
        YVal::Seq(s) => s.iter().filter_map(|x| x.as_str().map(str::to_owned)).collect(),
        YVal::Str(t) => t.src.lines().map(str::to_owned).collect(),
        _ => Vec::new(),
    };
    let mut with = vec![("subjects".to_owned(), YVal::Str(Tpl::new(lines.join("\n"), Default::default())))];
    if let Some(p) = provenance {
        with.push(("provenance".to_owned(), p));
    }
    if let Some(s) = sbom {
        with.push(("sbom".to_owned(), s));
    }
    Some(Step { name: Some(Tpl::new("Attest", Default::default())), uses: Some("kiln/attest@v1".into()), with, ..Step::default() })
}

/// SLSA v1.0 provenance (a predicate) for a build of `f`.
pub fn provenance(f: &Facts) -> Value {
    let mut deps = vec![json!({ "uri": format!("git+{}@{}", f.repository, f.git_ref), "digest": { "gitCommit": f.sha } })];
    for (uses, repo, sha) in &f.actions {
        let at = uses.split_once('@').map_or("", |(_, r)| r);
        deps.push(json!({ "uri": format!("git+https://github.com/{repo}@{at}"), "digest": { "gitCommit": sha }, "name": uses }));
    }
    for (image, digest) in &f.images {
        let (alg, hex) = digest.split_once(':').unwrap_or(("sha256", digest));
        deps.push(json!({ "uri": format!("docker://{image}"), "digest": { alg: hex } }));
    }
    let mut metadata = json!({ "invocationId": format!("{}/attempts/{}", f.run_url, f.attempt) });
    if let Some(o) = metadata.as_object_mut() {
        if let Some(s) = &f.started {
            o.insert("startedOn".into(), json!(s));
        }
        if let Some(s) = &f.finished {
            o.insert("finishedOn".into(), json!(s));
        }
    }
    json!({
        "buildDefinition": {
            "buildType": BUILD_TYPE,
            "externalParameters": {
                "workflow": { "repository": f.repository, "path": f.workflow_path, "ref": f.git_ref },
                "inputs": if f.inputs.is_null() { json!({}) } else { f.inputs.clone() },
            },
            "internalParameters": { "event": f.event },
            "resolvedDependencies": deps,
        },
        "runDetails": {
            "builder": { "id": f.builder_id },
            "metadata": metadata,
            "byproducts": [{ "name": "runner", "annotations": { "runner": f.runner, "executor": f.executor, "isolation": f.isolation } }],
        },
    })
}

/// DSSE's pre-authentication encoding of `payload`: what is signed.
pub fn pae(payload_type: &str, payload: &[u8]) -> Vec<u8> {
    let mut out = format!("DSSEv1 {} {payload_type} {} ", payload_type.len(), payload.len()).into_bytes();
    out.extend_from_slice(payload);
    out
}

/// A DSSE envelope of `payload`, with its signature (DER) by key `keyid`.
pub fn envelope(payload_type: &str, payload: &[u8], keyid: &str, sig: &[u8]) -> Value {
    let b64 = |b: &[u8]| base64::engine::general_purpose::STANDARD.encode(b);
    json!({ "payloadType": payload_type, "payload": b64(payload), "signatures": [{ "keyid": keyid, "sig": b64(sig) }] })
}

/// A Sigstore bundle (v0.3) of `envelope`, signed by the key `keyid` names.
pub fn bundle(envelope: Value, keyid: &str) -> Value {
    json!({ "mediaType": BUNDLE, "verificationMaterial": { "publicKey": { "hint": keyid } }, "dsseEnvelope": envelope })
}

/// The statement a bundle signs, when it is one.
pub fn statement_of(bundle: &Value) -> Option<Value> {
    let p = bundle.pointer("/dsseEnvelope/payload").and_then(Value::as_str)?;
    serde_json::from_slice(&base64::engine::general_purpose::STANDARD.decode(p).ok()?).ok()
}

/// What `pae` signed in a bundle, and the signature (DER): for verifying.
pub fn signed_of(bundle: &Value) -> Option<(Vec<u8>, Vec<u8>)> {
    let e = bundle.get("dsseEnvelope")?;
    let ty = e.get("payloadType").and_then(Value::as_str)?;
    let dec = |s: &str| base64::engine::general_purpose::STANDARD.decode(s).ok();
    let payload = dec(e.get("payload").and_then(Value::as_str)?)?;
    let sig = dec(e.pointer("/signatures/0/sig").and_then(Value::as_str)?)?;
    Some((pae(ty, &payload), sig))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn facts() -> Facts {
        Facts {
            builder_id: "https://kiln.example/ci@v1".into(),
            repository: "https://kiln.example/acme/app".into(),
            workflow_path: ".github/workflows/release.yml".into(),
            git_ref: "refs/tags/v1.2.0".into(),
            sha: "0123456789abcdef0123456789abcdef01234567".into(),
            inputs: json!({ "channel": "stable" }),
            event: "push".into(),
            actions: vec![("actions/checkout@v4".into(), "actions/checkout".into(), "11bd71901bbe5b1630ceea73d27597364c9af683".into())],
            images: vec![("node:22".into(), "sha256:aaaa".into())],
            run_url: "https://kiln.example/runs/r1".into(),
            attempt: 2,
            runner: "builder-1".into(),
            executor: "docker".into(),
            isolation: "container".into(),
            started: Some("2026-10-18T00:00:00Z".into()),
            finished: Some("2026-10-18T00:05:00Z".into()),
        }
    }

    #[test]
    fn w18_r01_provenance() {
        let p = provenance(&facts());
        assert_eq!(p.pointer("/buildDefinition/buildType").and_then(Value::as_str), Some(BUILD_TYPE));
        assert_eq!(p.pointer("/runDetails/builder/id").and_then(Value::as_str), Some("https://kiln.example/ci@v1"));
        assert_eq!(p.pointer("/buildDefinition/externalParameters/workflow/path").and_then(Value::as_str), Some(".github/workflows/release.yml"));
        assert_eq!(p.pointer("/buildDefinition/externalParameters/inputs/channel").and_then(Value::as_str), Some("stable"));
        let deps = p.pointer("/buildDefinition/resolvedDependencies").and_then(Value::as_array).cloned().unwrap_or_default();
        assert_eq!(deps.len(), 3, "the source, the action, the image");
        assert_eq!(deps.first().and_then(|d| d.pointer("/digest/gitCommit")).and_then(Value::as_str), Some("0123456789abcdef0123456789abcdef01234567"));
        assert_eq!(deps.get(1).and_then(|d| d.get("uri")).and_then(Value::as_str), Some("git+https://github.com/actions/checkout@v4"));
        assert_eq!(deps.get(2).and_then(|d| d.pointer("/digest/sha256")).and_then(Value::as_str), Some("aaaa"));
        assert_eq!(p.pointer("/runDetails/metadata/invocationId").and_then(Value::as_str), Some("https://kiln.example/runs/r1/attempts/2"));
        assert_eq!(p.pointer("/runDetails/byproducts/0/annotations/isolation").and_then(Value::as_str), Some("container"));
        let s = statement(&[Subject { name: "dist/app.tar.gz".into(), digest: [("sha256".to_owned(), "beef".to_owned())].into() }], PROVENANCE, p);
        assert_eq!((s.get("_type").and_then(Value::as_str), s.pointer("/subject/0/digest/sha256").and_then(Value::as_str)), (Some(STATEMENT), Some("beef")));
    }

    #[test]
    fn w18_r01_job_attest() {
        let t = |s: &str| YVal::Str(Tpl::new(s, Default::default()));
        let v = YVal::Map(vec![("subjects".into(), YVal::Seq(vec![t("dist/*.tar.gz"), t("ghcr.io/acme/app:1.2")])), ("sbom".into(), t("cyclonedx"))]);
        let s = job_step(&v).unwrap_or_default();
        assert_eq!(s.uses.as_deref(), Some("kiln/attest@v1"));
        assert_eq!(s.with.iter().map(|(k, v)| (k.as_str(), v.as_str().unwrap_or("?"))).collect::<Vec<_>>(), [("subjects", "dist/*.tar.gz\nghcr.io/acme/app:1.2"), ("sbom", "cyclonedx")]);
        assert_eq!(job_step(&t("out/app")).map(|s| s.with.len()), Some(1));
        assert!(job_step(&YVal::Bool(false)).is_none());
        assert_eq!(sbom_type(&json!({ "spdxVersion": "SPDX-2.3" })), Some(SPDX));
        assert_eq!(sbom_type(&json!({ "bomFormat": "CycloneDX" })), Some(CYCLONEDX));
        assert_eq!(sbom_type(&json!({ "_type": STATEMENT, "predicateType": PROVENANCE })), None, "a statement is no SBOM");
    }

    #[test]
    fn w18_r02_dsse() {
        // DSSE's own example of the encoding.
        assert_eq!(pae("http://example.com/HelloWorld", b"hello world"), b"DSSEv1 29 http://example.com/HelloWorld 11 hello world".to_vec());
        let e = envelope(PAYLOAD_TYPE, b"{}", "k1", b"sig");
        let b = bundle(e, "k1");
        assert_eq!(b.get("mediaType").and_then(Value::as_str), Some(BUNDLE));
        assert_eq!(statement_of(&b), Some(json!({})));
        assert_eq!(signed_of(&b), Some((pae(PAYLOAD_TYPE, b"{}"), b"sig".to_vec())));
    }
}
