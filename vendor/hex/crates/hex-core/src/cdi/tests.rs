use super::*;
use serde_json::{Value, json};

fn document(global: Value, devices: Value) -> Value {
    json!({"cdiVersion":"1.1.0", "kind":"vendor.example/gpu", "containerEdits":global, "devices":devices})
}
fn parse(value: &Value) -> Result<Spec, Error> {
    let bytes = serde_json::to_vec(value).unwrap();
    Spec::parse(&bytes, bytes.len())
}
fn device(name: &str, edits: Value) -> Value {
    json!({"name":name, "containerEdits":edits})
}
fn minimal() -> Value {
    document(json!({}), json!([device("0", json!({"env":["DEVICE=0"]}))]))
}

/// AC-T7.4.1: selected global/device data is exact and nonvacuous in all seven CDI edit families.
#[test]
fn selection_retains_all_edits_and_only_requested_devices() {
    let global = json!({
        "env":["MODE=global", "ESCAPED=a\n\"b\\c"],
        "deviceNodes":[{"path":"/dev/global", "hostPath":"/host/global", "type":"c", "major":195,"minor":0,"fileMode":432,"permissions":"rw","uid":10,"gid":20}],
        "mounts":[{"hostPath":"/lib/global","containerPath":"/lib/shared","type":"none","options":["ro","bind"]}],
        "hooks":[{"hookName":"createContainer","path":"/bin/global-hook","args":["hook","arg"],"env":["HOOK=yes"],"timeout":5}],
        "netDevices":[{"hostInterfaceName":"host0","name":"container0"}],
        "intelRdt":{"closID":"global","l3CacheSchema":"old","memBwSchema":"MB:0=10","schemata":[],"enableMonitoring":true},
        "additionalGids":[0,7]
    });
    let selected = json!({
        "env":["MODE=device"],
        "deviceNodes":[{"path":"/dev/selected","type":"u","major":0,"minor":-1,"fileMode":0,"permissions":"rwm","uid":0,"gid":0}],
        "mounts":[{"hostPath":"/lib/selected","containerPath":"/lib/shared","options":["bind","rw"]}],
        "hooks":[{"hookName":"poststop","path":"/bin/selected-hook"}],
        "netDevices":[{"hostInterfaceName":"host1","name":"container1"}],
        "intelRdt":{"closID":"selected","schemata":["L3:0=ff"],"enableMonitoring":false},
        "additionalGids":[8,7]
    });
    let mut value = document(
        global,
        json!([
            device("0", selected),
            device("unselected", json!({"env":["FORBIDDEN=not-selected"]})),
            device("last", json!({"env":["LAST=yes"]}))
        ]),
    );
    value["annotations"] = json!({"opaque/private":"metadata"});
    value["devices"][0]["annotations"] = json!({"opaque/device":"private"});
    let spec = parse(&value).unwrap();
    let edits = spec
        .select(&["vendor.example/gpu=0", "vendor.example/gpu=last"], 2)
        .unwrap();
    assert_eq!(
        edits.env,
        [
            "MODE=global",
            "ESCAPED=a\n\"b\\c",
            "MODE=device",
            "LAST=yes"
        ]
    );
    assert_eq!(edits.device_nodes.len(), 2);
    let global = edits.device_nodes[0];
    assert_eq!(
        (&*global.path, global.host_path.as_deref(), global.kind),
        ("/dev/global", Some("/host/global"), Some(DeviceType::Char))
    );
    assert_eq!(
        (
            global.major,
            global.minor,
            global.file_mode,
            global.uid,
            global.gid
        ),
        (Some(195), Some(0), Some(432), Some(10), Some(20))
    );
    assert_eq!(global.permissions.as_deref(), Some("rw"));
    let node = edits.device_nodes[1];
    assert_eq!(
        (
            node.kind,
            node.major,
            node.minor,
            node.file_mode,
            node.uid,
            node.gid
        ),
        (
            Some(DeviceType::UnbufferedChar),
            Some(0),
            Some(-1),
            Some(0),
            Some(0),
            Some(0)
        )
    );
    assert!(node.host_path.is_none());
    assert_eq!(node.permissions.as_deref(), Some("rwm"));
    assert_eq!(
        edits
            .mounts
            .iter()
            .map(|m| m.host_path.as_str())
            .collect::<Vec<_>>(),
        ["/lib/global", "/lib/selected"]
    );
    assert_eq!(edits.mounts[0].options, ["ro", "bind"]);
    assert_eq!(edits.mounts[0].kind.as_deref(), Some("none"));
    assert_eq!(edits.mounts[1].container_path, "/lib/shared");
    assert_eq!(edits.hooks.len(), 2);
    assert_eq!(
        (edits.hooks[0].hook_name, edits.hooks[0].timeout),
        (HookName::CreateContainer, Some(5))
    );
    assert_eq!(edits.hooks[0].args, ["hook", "arg"]);
    assert_eq!(edits.hooks[0].env, ["HOOK=yes"]);
    assert_eq!(
        (edits.hooks[1].hook_name, edits.hooks[1].path.as_str()),
        (HookName::Poststop, "/bin/selected-hook")
    );
    assert_eq!(
        edits
            .net_devices
            .iter()
            .map(|n| (n.host_interface_name.as_str(), n.name.as_str()))
            .collect::<Vec<_>>(),
        [("host0", "container0"), ("host1", "container1")]
    );
    let rdt = edits.intel_rdt.unwrap();
    assert_eq!(rdt.clos_id, "selected");
    assert!(rdt.l3_cache_schema.is_empty() && rdt.mem_bw_schema.is_empty());
    assert_eq!(rdt.schemata.as_ref().unwrap(), &["L3:0=ff"]);
    assert!(!rdt.enable_monitoring);
    assert_eq!(edits.additional_gids, [0, 7, 8, 7]); // Zero and repeated GIDs are data, not silently lost.
    assert_eq!(spec.raw.annotations[0].value, "metadata");
    assert_eq!(spec.raw.devices[0].annotations[0].value, "private");
    assert!(std::ptr::eq(
        edits.device_nodes[0],
        &spec.raw.container_edits.device_nodes[0]
    ));
}

/// AC-T7.4.1: Go Append order is request order, globals once; replacement RDT is whole-value.
#[test]
fn request_order_globals_once_and_last_rdt_match_go_append() {
    let spec = parse(&document(
        json!({"env":["X=global"],"intelRdt":{"closID":"global","l3CacheSchema":"schema"}}),
        json!([
            device("a", json!({"env":["X=a"],"intelRdt":{"closID":"a"}})),
            device("b", json!({"env":["X=b"],"intelRdt":{"closID":"b"}}))
        ]),
    ))
    .unwrap();
    let edits = spec
        .select(&["vendor.example/gpu=b", "vendor.example/gpu=a"], 2)
        .unwrap();
    assert_eq!(edits.env, ["X=global", "X=b", "X=a"]);
    assert_eq!(edits.intel_rdt.unwrap().clos_id, "a");
    assert!(edits.intel_rdt.unwrap().l3_cache_schema.is_empty());
    let empty = spec.select(&[], 0).unwrap();
    assert!(empty.env.is_empty() && empty.intel_rdt.is_none());
}

/// AC-T7.4.1: input and selection bounds refuse at the caller boundary, with no partial result.
#[test]
fn caller_bounds_are_exact_and_selection_failure_is_atomic() {
    let bytes = serde_json::to_vec(&minimal()).unwrap();
    assert_eq!(
        Spec::parse(&bytes, bytes.len() - 1).unwrap_err().cause,
        Cause::Limit {
            bound: bytes.len() - 1,
            actual: bytes.len()
        }
    );
    let spec = Spec::parse(&bytes, bytes.len()).unwrap();
    assert!(matches!(
        spec.select(&["vendor.example/gpu=0"], 0),
        Err(Error {
            field: Field::Selection,
            cause: Cause::Limit { .. },
            ..
        })
    ));
    assert_eq!(
        spec.select(&["vendor.example/gpu=0", "vendor.example/gpu=0"], 2)
            .unwrap_err()
            .cause,
        Cause::Duplicate
    );
    assert_eq!(
        spec.select(&["vendor.example/gpu=0", "vendor.example/gpu=missing"], 2)
            .unwrap_err()
            .item,
        Some(1)
    );
    assert_eq!(
        spec.select(&["other.example/gpu=0"], 1).unwrap_err().cause,
        Cause::Unresolved
    );
    assert_eq!(
        spec.select(&["vendor.example/gpu=0"], 1).unwrap().env,
        ["DEVICE=0"]
    );
}

/// AC-T7.4.1: reject unknown/duplicate JSON fields at every edit seam without logging values.
#[test]
fn unknown_duplicate_and_wrong_json_types_are_typed() {
    let duplicate =
        br#"{"cdiVersion":"1.1.0","cdiVersion":"1.1.0","kind":"vendor.example/gpu","devices":[]}"#;
    assert_eq!(
        Spec::parse(duplicate, duplicate.len()).unwrap_err().cause,
        Cause::DuplicateField
    );
    for edits in [
        json!({"unknown-private-field":{"nested":[[[]]]}}),
        json!({"hooks":[{"hookName":"poststop","path":"/hook","secret-unknown":"private"}]}),
        json!({"mounts":[{"hostPath":"/h","containerPath":"/c","unknown":true}]}),
        json!({"deviceNodes":[{"path":"/dev/d","unknown":0}]}),
        json!({"netDevices":[{"hostInterfaceName":"h","name":"n","unknown":false}]}),
        json!({"intelRdt":{"schema":["misspelled"]}}),
        json!({"additionalGIDs":[3]}),
    ] {
        assert_eq!(
            parse(&document(json!({}), json!([device("0", edits)])))
                .unwrap_err()
                .cause,
            Cause::UnknownField
        );
    }
    for value in [
        json!([]),
        json!({}),
        json!({"cdiVersion":1}),
        json!("private-payload"),
    ] {
        assert!(parse(&value).is_err());
    }
    let mut value = minimal();
    value["devices"][0]["containerEdits"]["env"] = json!([7]);
    assert!(parse(&value).is_err());
    let mut trailing = serde_json::to_vec(&minimal()).unwrap();
    trailing.extend_from_slice(b" private-tail");
    assert_eq!(
        Spec::parse(&trailing, trailing.len()).unwrap_err().cause,
        Cause::MalformedJson
    );
}

/// AC-T7.4.1: device membership and names use pinned Go parser grammar, not a vendor shortcut.
#[test]
fn qualified_grammar_membership_and_duplicates_are_checked() {
    for valid in [
        "nvidia.com/gpu=GPU-123",
        "amd.com/gpu=0000:01:00.0",
        "A/B=a",
        "A_B/C.D=0",
    ] {
        let (kind, name) = valid.split_once('=').unwrap();
        let mut value = document(json!({}), json!([device(name, json!({"env":["X=1"]}))]));
        value["kind"] = json!(kind);
        assert!(
            parse(&value).unwrap().select(&[valid], 1).is_ok(),
            "{valid}"
        );
    }
    let spec = parse(&minimal()).unwrap();
    for invalid in [
        "gpu=0",
        "/gpu=0",
        "1vendor/gpu=0",
        "vendor.example/1gpu=0",
        "vendor.example/gpu=/0",
        "vendor.example/gpu=0:",
        "vendor.example/gpu=0=1",
        "vendor.example/gpu/=0",
        "vendor.example/gpu=é",
    ] {
        assert!(spec.select(&[invalid], 1).is_err(), "{invalid}");
    }
    let duplicate = document(
        json!({}),
        json!([
            device("0", json!({"env":["A=1"]})),
            device("0", json!({"env":["B=2"]}))
        ]),
    );
    assert_eq!(parse(&duplicate).unwrap_err().cause, Cause::Duplicate);
    let duplicate_metadata = br#"{"cdiVersion":"1.1.0","kind":"vendor.example/gpu","annotations":{"x":"private1","x":"private2"},"devices":[{"name":"0","containerEdits":{"env":["A=1"]}}]}"#;
    assert_eq!(
        Spec::parse(duplicate_metadata, duplicate_metadata.len())
            .unwrap_err()
            .cause,
        Cause::Duplicate
    );
    assert_eq!(
        parse(&document(json!({}), json!([]))).unwrap_err().cause,
        Cause::Empty
    );
    assert_eq!(
        parse(&document(json!({}), json!([device("0", json!({}))])))
            .unwrap_err()
            .cause,
        Cause::Empty
    );
}

/// AC-T7.4.1: known released versions reject newer features; do not accept a future schema.
#[test]
fn versions_and_minimum_feature_gates_match_the_pinned_source() {
    for version in [
        "0.3.0", "0.4.0", "0.5.0", "0.6.0", "0.7.0", "0.8.0", "1.0.0", "1.1.0",
    ] {
        let mut value = document(json!({}), json!([device("a", json!({"env":["A=1"]}))]));
        value["cdiVersion"] = json!(version);
        assert!(parse(&value).is_ok(), "{version}");
    }
    for version in [
        "0.1.0",
        "0.2.0",
        "v1.1.0",
        "1.2.0",
        "01.1.0",
        "1.1",
        "1.1.0-rc.1",
    ] {
        let mut value = minimal();
        value["cdiVersion"] = json!(version);
        assert_eq!(parse(&value).unwrap_err().cause, Cause::UnsupportedVersion);
    }
    for (old, new, edit) in [
        (
            "0.3.0",
            "0.4.0",
            json!({"mounts":[{"hostPath":"/h","containerPath":"/c","type":"none"}]}),
        ),
        (
            "0.4.0",
            "0.5.0",
            json!({"deviceNodes":[{"path":"/d","hostPath":"/h"}]}),
        ),
        ("0.6.0", "0.7.0", json!({"additionalGids":[7]})),
        ("0.6.0", "0.7.0", json!({"intelRdt":{"closID":"id"}})),
        (
            "1.0.0",
            "1.1.0",
            json!({"netDevices":[{"hostInterfaceName":"h","name":"n"}]}),
        ),
        ("1.0.0", "1.1.0", json!({"intelRdt":{"schemata":[]}})),
        (
            "1.0.0",
            "1.1.0",
            json!({"intelRdt":{"enableMonitoring":true}}),
        ),
    ] {
        let mut value = document(json!({}), json!([device("a", edit)]));
        value["cdiVersion"] = json!(old);
        assert!(
            matches!(
                parse(&value),
                Err(Error {
                    cause: Cause::Requires(_),
                    ..
                })
            ),
            "{old}"
        );
        value["cdiVersion"] = json!(new);
        assert!(parse(&value).is_ok(), "{new}");
    }
    for patch in [
        json!({"annotations":{"k":"v"}}),
        json!({"kind":"vendor.example/g.pu"}),
    ] {
        let mut value = document(json!({}), json!([device("a", json!({"env":["A=1"]}))]));
        value
            .as_object_mut()
            .unwrap()
            .extend(patch.as_object().unwrap().clone());
        value["cdiVersion"] = json!("0.5.0");
        assert!(matches!(
            parse(&value),
            Err(Error {
                cause: Cause::Requires(Version::V06),
                ..
            })
        ));
    }
}

/// AC-T7.4.1: reject malformed supported edits; retain repeated env/path operations as CDI does.
#[test]
fn edit_validation_does_not_invent_cross_object_conflicts() {
    for edit in [
        json!({"env":["MISSING_EQUALS"]}),
        json!({"env":["=value"]}),
        json!({"hooks":[{"hookName":"unsupported-private-hook","path":"/hook"}]}),
        json!({"hooks":[{"hookName":"poststop","path":"relative"}]}),
        json!({"hooks":[{"hookName":"poststop","path":"/hook","timeout":0}]}),
        json!({"hooks":[{"hookName":"poststop","path":"/hook","timeout":-1}]}),
        json!({"deviceNodes":[{"path":"/d","type":"invalid"}]}),
        json!({"deviceNodes":[{"path":"/d","permissions":"rwx"}]}),
        json!({"deviceNodes":[{"path":"/d","uid":-1}]}),
        json!({"deviceNodes":[{"path":"/d","fileMode":4294967296u64}]}),
        json!({"mounts":[{"hostPath":"","containerPath":"/c"}]}),
        json!({"netDevices":[{"hostInterfaceName":"h","name":"a"},{"hostInterfaceName":"h","name":"b"}]}),
        json!({"netDevices":[{"hostInterfaceName":"h1","name":"a"},{"hostInterfaceName":"h2","name":"a"}]}),
        json!({"intelRdt":{"closID":"../private"}}),
    ] {
        assert!(parse(&document(json!({}), json!([device("a", edit)]))).is_err());
    }
    let spec=parse(&document(json!({"env":["X=global"],"netDevices":[{"hostInterfaceName":"h","name":"old"}]}),json!([device("a",json!({"env":["X=one","X=two"],"netDevices":[{"hostInterfaceName":"h","name":"new"}],"deviceNodes":[{"path":"/d","type":"","permissions":"rrw"}]}))]))).unwrap();
    let edits = spec.select(&["vendor.example/gpu=a"], 1).unwrap();
    assert_eq!(edits.env, ["X=global", "X=one", "X=two"]);
    assert_eq!(edits.net_devices.len(), 2);
    assert_eq!(edits.device_nodes[0].kind, Some(DeviceType::Unspecified));
    assert_eq!(edits.device_nodes[0].permissions.as_deref(), Some("rrw"));
}

/// AC-T7.4.1: diagnostics and exposed Debug do not disclose private field names/paths/env/bytes.
#[test]
fn private_content_is_absent_from_error_and_debug_ergonomics() {
    let secret = "PRIVATE-sentinel-escapes-\n-path-env";
    let spec=parse(&document(json!({"env":[format!("SECRET={secret}")]}),json!([device("a",json!({"mounts":[{"hostPath":secret,"containerPath":"/c"}],"hooks":[{"hookName":"poststop","path":"/secret"}],"intelRdt":{"closID":"private"}}))]))).unwrap();
    let edits = spec.select(&["vendor.example/gpu=a"], 1).unwrap();
    for debug in [
        format!("{spec:?}"),
        format!("{edits:?}"),
        format!("{:?}", edits.mounts[0]),
        format!("{:?}", edits.hooks[0]),
        format!("{:?}", edits.intel_rdt.unwrap()),
    ] {
        assert!(
            !debug.contains("PRIVATE") && !debug.contains("secret") && !debug.contains("SECRET")
        );
    }
    for value in [
        json!({secret:secret}),
        document(json!({}), json!([device("a", json!({"env":[secret]}))])),
        document(
            json!({}),
            json!([device(
                "a",
                json!({"hooks":[{"hookName":secret,"path":"/hook"}]})
            )]),
        ),
    ] {
        let error = parse(&value).unwrap_err();
        let formatted = format!("{error:?} {error}");
        assert!(!formatted.contains("PRIVATE") && !formatted.contains("sentinel"));
    }
    let error = spec.select(&[secret], 1).unwrap_err();
    assert!(!format!("{error:?} {error}").contains("PRIVATE"));
}
