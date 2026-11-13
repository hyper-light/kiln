use super::super::GpuObserver;
use super::*;
use serde_json::{Value, json};

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

fn schema(input: &GpuObserverInput) -> Value {
    json!({
        "evidence": hex(&input.evidence),
        "artifacts": input.artifacts.iter().map(|artifact| json!({
            "role": match artifact.role {
                GpuArtifactRole::Program => "program",
                GpuArtifactRole::Interpreter => "interpreter",
                GpuArtifactRole::Loader => "loader",
                GpuArtifactRole::Icd => "icd",
                GpuArtifactRole::Driver => "driver",
                GpuArtifactRole::Dependency => "dependency",
            },
            "host": artifact.host,
            "guest": artifact.guest,
            "bytes": artifact.bytes,
            "sha256": hex(&artifact.sha256),
        })).collect::<Vec<_>>(),
        "launch": input.launch,
        "max_queue_families": input.max_queue_families,
        "max_allocation_bytes": input.max_allocation_bytes,
        "bounds": input.bounds,
    })
}

fn file(path: &Path, bytes: &[u8]) -> [u8; 32] {
    std::fs::write(path, bytes).unwrap();
    Sha256::digest(bytes).into()
}

/// AC-T7.4.1: explicit files round-trip escaped/Unicode paths and the SAME
/// constructor verifies their actual bytes/RO mappings/paid envelope. Fixture
/// bytes are hash/mapping models, not executable GPU artifacts or a VM result.
#[test]
fn optional_observer_manifest_reaches_real_constructor_with_escaped_unicode_paths() {
    let dir = tempfile::tempdir().unwrap();
    let unicode = dir.path().join("unicode-Ω-😀");
    std::fs::create_dir(&unicode).unwrap();
    let (config, input, allocation) = super::super::tests::fixture(&unicode);
    let text = serde_json::to_string_pretty(&schema(&input))
        .unwrap()
        .replace('Ω', "\\u03a9")
        .replace('😀', "\\ud83d\\ude00")
        .replace('/', "\\/");
    let path = dir.path().join("observer.json");
    let expected_sha256 = file(&path, text.as_bytes());
    let loaded = GpuObserverInput::from_vars(|name| match name {
        ENV_GPU_OBSERVER_FILE => Some(path.as_os_str().to_owned()),
        ENV_GPU_OBSERVER_SHA256 => Some(hex(&expected_sha256).to_uppercase().into()),
        _ => panic!("unexpected observer environment input"),
    })
    .unwrap()
    .unwrap();
    assert_eq!(loaded.artifacts.len(), input.artifacts.len());
    for (actual, expected) in loaded.artifacts.iter().zip(&input.artifacts) {
        assert_eq!(actual.host, expected.host);
        assert_eq!(actual.guest, expected.guest);
        assert_eq!(actual.sha256, expected.sha256);
    }
    assert_eq!(loaded.launch, input.launch);
    assert_eq!(loaded.bounds, input.bounds);
    let expected = GpuObserver::new(input, &config, allocation).unwrap();
    let actual = GpuObserver::new(loaded, &config, allocation).unwrap();
    assert_eq!(actual.identity(), expected.identity());
    let actual_program = actual.program(1).unwrap();
    let expected_program = expected.program(1).unwrap();
    assert_eq!(actual_program.args, expected_program.args);
    assert_eq!(actual_program.env, expected_program.env);
    assert!(GpuObserverInput::from_vars(|_| None).unwrap().is_none());
    for present in [ENV_GPU_OBSERVER_FILE, ENV_GPU_OBSERVER_SHA256] {
        assert!(
            GpuObserverInput::from_vars(|name| {
                if name == present {
                    Some(path.as_os_str().to_owned())
                } else {
                    None
                }
            })
            .is_err()
        );
    }
    // JSON string escapes are supported even where those decoded characters
    // are later unsuitable for a particular artifact/path/launch role.
    let mut json = Json {
        rest: r#""a\"\\\/\b\f\n\r\t\u03a9\ud83d\ude00""#,
        bytes: 64,
    };
    assert_eq!(json.string().unwrap(), "a\"\\/\u{0008}\u{000c}\n\r\tΩ😀");
    assert!(json.rest.is_empty());
}

/// AC-T7.4.1: write a manifest that names a stderr bound beside its others and expect it refused as an
/// unknown field: the Program's stderr bound is the node's own (observer::stderr_room), never a second,
/// configured one. The same manifest without it is read.
#[test]
fn observer_manifest_naming_a_stderr_bound_is_refused() {
    let dir = tempfile::tempdir().unwrap();
    let (_, input, _) = super::super::tests::fixture(dir.path());
    let valid = schema(&input);
    assert!(parse(&serde_json::to_string(&valid).unwrap()).is_ok());
    let mut named = valid;
    named
        .as_object_mut()
        .unwrap()
        .insert("stderr_bytes".into(), json!(4096));
    assert!(parse(&serde_json::to_string(&named).unwrap()).is_err());
}

/// AC-T7.4.1: schema/byte/field/count/UTF8 failures never create a default
/// observer; metadata-only file validation starts no worker, provider or VM.
#[test]
fn observer_manifest_rejects_hostile_schema_lengths_escapes_and_partial_inputs() {
    let dir = tempfile::tempdir().unwrap();
    let (_, input, _) = super::super::tests::fixture(dir.path());
    let valid = schema(&input);
    let path = dir.path().join("observer.json");
    let good = serde_json::to_vec(&valid).unwrap();
    let hash = file(&path, &good);
    assert!(GpuObserverInput::load(&path, hash).is_ok());
    assert!(GpuObserverInput::load(&path, [0; 32]).is_err());
    assert!(GpuObserverInput::load(Path::new("observer.json"), hash).is_err());
    assert!(GpuObserverInput::load(dir.path(), hash).is_err());
    for key in [
        "evidence",
        "artifacts",
        "launch",
        "max_queue_families",
        "max_allocation_bytes",
        "bounds",
    ] {
        let mut missing = valid.clone();
        missing.as_object_mut().unwrap().remove(key);
        assert!(
            parse(&serde_json::to_string(&missing).unwrap()).is_err(),
            "missing{key}"
        );
    }
    for text in [
        "{}".to_owned(),
        format!(
            "{{\"evidence\":\"{}\",\"\\u0065vidence\":\"{}\"}}",
            "ab".repeat(32),
            "cd".repeat(32)
        ),
        format!(
            "{{\"unknown\":{},\"evidence\":\"{}\"}}",
            "[".repeat(64),
            "ab".repeat(32)
        ),
        "{\"launch\":[\"\\ud800\"]}".to_owned(),
        "{\"launch\":[\"\\udc00\"]}".to_owned(),
        "{\"launch\":[\"\\ud800\\u0041\"]}".to_owned(),
        "{\"launch\":[\"\\x20\"]}".to_owned(),
        "{\"launch\":[\"line\nfeed\"]}".to_owned(),
        "{\"launch\":[{}]}".to_owned(),
        "{\"max_queue_families\":4294967296}".to_owned(),
        "{\"max_allocation_bytes\":18446744073709551616}".to_owned(),
        "{\"max_queue_families\":01}".to_owned(),
        "{\"max_queue_families\":1e2}".to_owned(),
        "{\"max_queue_families\":-1}".to_owned(),
        "{\"bounds\":{\"cpu_weight\":null,\"cpu_weight\":null}}".to_owned(),
        "{\"bounds\":{\"cpu_weight\":65536}}".to_owned(),
        "{\"artifacts\":[{\"role\":\"foreign\"}]}".to_owned(),
        format!("{}false", std::str::from_utf8(&good).unwrap()),
    ] {
        assert!(parse(&text).is_err(), "{text}");
    }
    let mut unknown = valid.clone();
    unknown
        .as_object_mut()
        .unwrap()
        .insert("default".into(), true.into());
    assert!(parse(&serde_json::to_string(&unknown).unwrap()).is_err());
    assert!(parse(&"[".repeat(MAX_BYTES + 1)).is_err());
    assert!(GpuObserverInput::load(&path, file(&path, &[0xff])).is_err());
    let opened = std::fs::File::create(&path).unwrap();
    opened
        .set_len(u64::try_from(MAX_BYTES).unwrap() + 1)
        .unwrap();
    assert!(GpuObserverInput::load(&path, hash).is_err());
    #[cfg(unix)]
    {
        let link = dir.path().join("observer-link.json");
        std::os::unix::fs::symlink(&path, &link).unwrap();
        assert!(GpuObserverInput::load(&link, hash).is_err());
    }
}

/// AC-T7.4.1: a correctly pinned config is no authority to substitute artifacts,
/// writable mappings, missing execution limits or an unpaid observer envelope.
#[test]
fn loaded_observer_manifest_keeps_actual_artifact_and_paid_room_checks() {
    let dir = tempfile::tempdir().unwrap();
    let (mut config, input, allocation) = super::super::tests::fixture(dir.path());
    let path = dir.path().join("observer.json");
    let valid = schema(&input);
    let hash = file(&path, &serde_json::to_vec(&valid).unwrap());
    config.shares.first_mut().unwrap().read_only = false;
    assert!(
        GpuObserver::new(
            GpuObserverInput::load(&path, hash).unwrap(),
            &config,
            allocation
        )
        .is_err()
    );
    config.shares.first_mut().unwrap().read_only = true;
    for (field, value) in [
        ("pids_max", Value::Null),
        ("cpu_max_milli", Value::Null),
        ("memory_max", Value::Null),
        (
            "memory_max",
            json!(allocation.guest().unwrap().mem_bytes + 1),
        ),
    ] {
        let mut invalid = valid.clone();
        invalid["bounds"][field] = value;
        let hash = file(&path, &serde_json::to_vec(&invalid).unwrap());
        assert!(
            GpuObserver::new(
                GpuObserverInput::load(&path, hash).unwrap(),
                &config,
                allocation
            )
            .is_err()
        );
    }
    let hash = file(&path, &serde_json::to_vec(&valid).unwrap());
    let program = input.artifacts.first().unwrap();
    std::fs::write(&program.host, b"changed actual artifact bytes").unwrap();
    assert!(
        GpuObserver::new(
            GpuObserverInput::load(&path, hash).unwrap(),
            &config,
            allocation
        )
        .is_err()
    );
}
