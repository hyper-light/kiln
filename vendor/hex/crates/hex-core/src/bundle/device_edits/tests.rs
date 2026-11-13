use super::*;
use hex_api::{Mount as Bind, Program, User};
use serde_json::{Value, json};

fn spec() -> OciSpec {
    OciSpec {
        rootfs: Rootfs::Dir("/image".into()),
        init: Program::of(&["/bin/true"]),
        mounts: Vec::new(),
        hostname: None,
        network: Network::Private,
        hosts: Vec::new(),
    }
}

fn place() -> Place<'static> {
    Place {
        dir: Path::new("/bundles/fixture"),
        owner: Owner::Root,
        masked: &[],
    }
}

fn selected(data: Value) -> cdi::Spec {
    let bytes = serde_json::to_vec(&data).unwrap();
    cdi::Spec::parse(&bytes, bytes.len()).unwrap()
}

fn one(edits: Value) -> cdi::Spec {
    selected(json!({"cdiVersion":"1.1.0", "kind":"test.example/gpu",
        "devices":[{"name":"one", "containerEdits":edits}]}))
}

fn receipt(node: &cdi::DeviceNode, kind: DeviceType, major: u32, minor: u32) -> ResolvedNode<'_> {
    ResolvedNode {
        node,
        kind,
        major,
        minor,
        file_mode: 0o2660,
    }
}

fn config(spec: &OciSpec, edits: &cdi::Edits<'_>, nodes: &[ResolvedNode<'_>]) -> Value {
    serde_json::from_slice(
        &render_with_devices(spec, &place(), edits, nodes)
            .unwrap()
            .config,
    )
    .unwrap()
}

/// AC-T7.1.1 / AC-T7.3.1: ordinary callers use the very same renderer and get no new fields,
/// sorting or group/env changes. The remote before/after witness also compares d4d bytes.
#[test]
fn empty_edits_use_the_existing_renderer_and_preserve_auxiliary_data() {
    for owner in [
        Owner::Root,
        Owner::Rootless {
            uid: 1000,
            gid: 1000,
        },
    ] {
        for rootfs in [
            Rootfs::Dir("/image".into()),
            Rootfs::Layers(vec!["/base".into(), "/app".into()]),
        ] {
            let spec = OciSpec { rootfs, ..spec() };
            let at = Place { owner, ..place() };
            let ordinary = render(&spec, &at).unwrap();
            let with = render_with_devices(&spec, &at, &cdi::Edits::default(), &[]).unwrap();
            assert_eq!(ordinary, with);
            let value: Value = serde_json::from_slice(&with.config).unwrap();
            assert!(value.get("hooks").is_none());
            assert!(value["linux"].get("devices").is_none());
            assert_eq!(
                value["linux"]["resources"]["devices"],
                json!([{"allow":false,"access":"rwm"}])
            );
        }
    }
}

/// AC-T7.3.1: actual selector output applies globals once, selected requests only and in order.
/// The first CDI key appends beside original env; later CDI same-key edits replace that slot:
/// runtime-tools edf4cb3d NewFromSpec/createEnvCacheMap + addEnv, not an invented merge rule.
#[test]
fn selected_globals_and_env_follow_the_pinned_generator() {
    let parsed = selected(json!({"cdiVersion":"1.1.0", "kind":"test.example/gpu",
    "containerEdits":{"env":["BASE=global", "GLOBAL=once"]},
    "devices":[
        {"name":"one","containerEdits":{"env":["BASE=one","A=first"]}},
        {"name":"two","containerEdits":{"env":["BASE=two","A=last","B=two"]}},
        {"name":"unselected","containerEdits":{"env":["HIDDEN=never"]}}
    ]}));
    let edits = parsed
        .select(&["test.example/gpu=two", "test.example/gpu=one"], 2)
        .unwrap();
    let mut spec = spec();
    spec.init.env = vec![
        ("BASE".into(), "original".into()),
        ("KEEP".into(), "yes".into()),
    ];
    let got = config(&spec, &edits, &[]);
    assert_eq!(
        got["process"]["env"],
        json!([
            "BASE=original",
            "KEEP=yes",
            "BASE=one",
            "GLOBAL=once",
            "A=first",
            "B=two"
        ])
    );
    let no_selection = parsed.select(&[], 0).unwrap();
    assert_eq!(
        render_with_devices(&spec, &place(), &no_selection, &[]).unwrap(),
        render(&spec, &place()).unwrap()
    );
}

/// AC-T7.3.1: host facts fill only Go's missing fields; explicit node identity/mode/zero IDs win.
/// Replacing a node path retains previous precise grants, exactly as Apply's append-only rules.
#[test]
fn nodes_preserve_explicit_fields_defaults_replacement_and_precise_grants() {
    let parsed = one(json!({"deviceNodes":[
        {"path":"/dev/selected", "hostPath":"/host/a", "minor":999},
        {"path":"/dev/block", "type":"b", "major":8, "minor":7, "fileMode":384, "uid":0, "gid":4, "permissions":"r"},
        {"path":"/dev/selected", "type":"c", "major":195, "minor":2, "permissions":"wm"},
        {"path":"/dev/fifo", "type":"p", "major":3, "minor":4},
        {"path":"/dev/unbuffered", "type":"u", "major":9, "minor":6}
    ]}));
    let edits = parsed.select(&["test.example/gpu=one"], 1).unwrap();
    let nodes = [
        receipt(edits.device_nodes[0], DeviceType::Char, 226, 128),
        receipt(edits.device_nodes[1], DeviceType::Char, 999, 999),
        receipt(edits.device_nodes[2], DeviceType::Char, 226, 129),
        receipt(edits.device_nodes[3], DeviceType::Fifo, 0, 0),
        receipt(edits.device_nodes[4], DeviceType::Char, 9, 6),
    ];
    let mut spec = spec();
    spec.init.user = Some(User {
        uid: 1000,
        gid: 2000,
        groups: vec![],
    });
    let got = config(&spec, &edits, &nodes);
    assert_eq!(
        got["linux"]["devices"],
        json!([
            {"path":"/dev/block","type":"b","major":8,"minor":7,"fileMode":384,"uid":0,"gid":4},
            {"path":"/dev/selected","type":"c","major":195,"minor":2,"fileMode":0o2660,"uid":1000,"gid":2000},
            {"path":"/dev/fifo","type":"p","major":3,"minor":4,"fileMode":0o2660,"uid":1000,"gid":2000},
            {"path":"/dev/unbuffered","type":"u","major":9,"minor":6,"fileMode":0o2660,"uid":1000,"gid":2000}
        ])
    );
    assert_eq!(
        got["linux"]["resources"]["devices"],
        json!([
            {"allow":false,"access":"rwm"},
            {"allow":true,"type":"c","major":226,"minor":128,"access":"rwm"},
            {"allow":true,"type":"b","major":8,"minor":7,"access":"r"},
            {"allow":true,"type":"c","major":195,"minor":2,"access":"wm"}
        ])
    );
    let root = config(&super::tests::spec(), &edits, &nodes);
    assert!(root["linux"]["devices"][1].get("uid").is_none());
    assert!(root["linux"]["devices"][1].get("gid").is_none());
    assert_eq!(
        nodes[0].node.minor,
        Some(999),
        "the selected source stays immutable"
    );
}

/// AC-T7.3.1: every selected operation requires its exact ordered host receipt. Invalid receipts
/// and a non-minimal specified type disagreeing with the host refuse the whole bundle.
#[test]
fn resolution_count_order_identity_type_and_mode_refuse_before_output() {
    let parsed = one(json!({"deviceNodes":[{"path":"/dev/a"},{"path":"/dev/b"}]}));
    let edits = parsed.select(&["test.example/gpu=one"], 1).unwrap();
    let nodes = [
        receipt(edits.device_nodes[0], DeviceType::Char, 1, 3),
        receipt(edits.device_nodes[1], DeviceType::Char, 1, 5),
    ];
    for bad in [
        Vec::new(),
        vec![receipt(edits.device_nodes[0], DeviceType::Char, 1, 3)],
        vec![
            receipt(edits.device_nodes[1], DeviceType::Char, 1, 5),
            receipt(edits.device_nodes[0], DeviceType::Char, 1, 3),
        ],
    ] {
        assert!(matches!(
            render_with_devices(&spec(), &place(), &edits, &bad),
            Err(Refusal::Invalid { .. })
        ));
    }
    let copied: cdi::DeviceNode = serde_json::from_value(json!({"path":"/dev/a"})).unwrap();
    let wrong = [
        receipt(&copied, DeviceType::Char, 1, 3),
        receipt(edits.device_nodes[1], DeviceType::Char, 1, 5),
    ];
    assert!(matches!(
        render_with_devices(&spec(), &place(), &edits, &wrong),
        Err(Refusal::Invalid {
            what: "cdi resolution",
            ..
        })
    ));
    for kind in [DeviceType::Unspecified, DeviceType::UnbufferedChar] {
        let wrong = [
            receipt(edits.device_nodes[0], kind, 1, 3),
            receipt(edits.device_nodes[1], DeviceType::Char, 1, 5),
        ];
        assert!(render_with_devices(&spec(), &place(), &edits, &wrong).is_err());
    }
    let wrong_mode = [
        ResolvedNode {
            file_mode: 0o20660,
            ..receipt(edits.device_nodes[0], DeviceType::Char, 1, 3)
        },
        receipt(edits.device_nodes[1], DeviceType::Char, 1, 5),
    ];
    assert!(render_with_devices(&spec(), &place(), &edits, &wrong_mode).is_err());
    assert_eq!(
        config(&spec(), &edits, &nodes)["linux"]["devices"]
            .as_array()
            .unwrap()
            .len(),
        2
    );
    let typed = one(json!({"deviceNodes":[{"path":"/dev/type", "type":"b", "major":0}]}));
    let edits = typed.select(&["test.example/gpu=one"], 1).unwrap();
    let wrong = [receipt(edits.device_nodes[0], DeviceType::Char, 8, 0)];
    assert!(matches!(
        render_with_devices(&spec(), &place(), &edits, &wrong),
        Err(Refusal::Invalid {
            what: "cdi deviceNodes.type",
            ..
        })
    ));
}

/// AC-T7.1.1 / AC-T7.3.1: remove FIRST exact mount destination, append, stable-sort ALL mounts by
/// Linux-cleaned depth. Equal-depth ordering and original destination bytes remain intact.
#[test]
fn mounts_replace_first_match_and_sort_stably_by_linux_cleaned_depth() {
    let parsed = one(json!({"mounts":[
        {"hostPath":"/host/leaf","containerPath":"/selected/nested/leaf","type":"bind","options":["rbind","ro"]},
        {"hostPath":"/host/parent","containerPath":"/selected","type":"bind"},
        {"hostPath":"/host/new","containerPath":"/duplicate","type":"bind"},
        {"hostPath":"/host/clean","containerPath":"/a/../clean//./","type":"bind"},
        {"hostPath":"/host/dev","containerPath":"/dev","type":"tmpfs","options":["mode=0700"]}
    ]}));
    let edits = parsed.select(&["test.example/gpu=one"], 1).unwrap();
    let mut spec = spec();
    for source in ["/first", "/second"] {
        spec.mounts.push(Bind {
            source: source.into(),
            destination: "/duplicate".into(),
            read_only: false,
        });
    }
    let got = config(&spec, &edits, &[]);
    let mounts = got["mounts"].as_array().unwrap();
    let dups: Vec<_> = mounts
        .iter()
        .filter(|m| m["destination"] == "/duplicate")
        .collect();
    assert_eq!(dups.len(), 2);
    assert_eq!(dups[0]["source"], "/second");
    assert_eq!(dups[1]["source"], "/host/new");
    let pos = |path| {
        mounts
            .iter()
            .position(|m| m["destination"] == path)
            .unwrap()
    };
    assert!(pos("/selected") < pos("/selected/nested/leaf"));
    assert!(pos("/a/../clean//./") < pos("/dev"));
    assert!(pos("/dev") < pos("/dev/pts"));
    assert_eq!(
        mounts[pos("/selected/nested/leaf")]["options"],
        json!(["rbind", "ro"])
    );
    for (path, want) in [
        ("/", 1),
        ("//a///b/../", 1),
        ("/../../a/b", 2),
        ("a/b/../../..", 0),
        ("../a/b", 2),
        ("./", 0),
        ("/a\\b/c", 2),
    ] {
        assert_eq!(linux_depth(path).unwrap(), want, "{path}");
    }
}

/// AC-T7.3.1: each hook phase preserves all selected arguments/env/timeout, and additional GIDs
/// skip0/deduplicate while retaining existing process groups (Go Apply + AddProcessAdditionalGid).
#[test]
fn all_hook_phases_and_additional_groups_reach_the_oci_config() {
    let mut hooks: Vec<Value> = ["prestart","createRuntime","createContainer","startContainer","poststart","poststop"].iter().map(|phase|
        json!({"hookName":phase,"path":"/hooks/run","args":["run","--selected"],"env":["KEY=private"],"timeout":7})).collect();
    hooks.push(json!({"hookName":"prestart","path":"/hooks/second"}));
    let parsed = one(json!({"hooks":hooks,"additionalGids":[0,20,30,30,10,40]}));
    let edits = parsed.select(&["test.example/gpu=one"], 1).unwrap();
    let mut spec = spec();
    spec.init.user = Some(User {
        uid: 1,
        gid: 2,
        groups: vec![10, 20],
    });
    let got = config(&spec, &edits, &[]);
    for phase in [
        "prestart",
        "createRuntime",
        "createContainer",
        "startContainer",
        "poststart",
        "poststop",
    ] {
        assert_eq!(
            got["hooks"][phase][0],
            json!({"path":"/hooks/run","args":["run","--selected"],"env":["KEY=private"],"timeout":7})
        );
    }
    assert_eq!(got["hooks"]["prestart"][1], json!({"path":"/hooks/second"}));
    assert_eq!(
        got["process"]["user"]["additionalGids"],
        json!([10, 20, 30, 40])
    );
    let only_zero = cdi::Edits {
        additional_gids: vec![0],
        ..Default::default()
    };
    assert_eq!(
        render_with_devices(&spec, &place(), &only_zero, &[]).unwrap(),
        render(&spec, &place()).unwrap()
    );
}

/// AC-T7.3.1: selected network/RDT edits have no native enforcement yet. Refuse BEFORE producing
/// output, even if another input is invalid. A pure codec retaining data is not native support.
#[test]
fn unsupported_native_edits_refuse_instead_of_emitting_ignored_fields() {
    for (data, field) in [
        (
            json!({"netDevices":[{"hostInterfaceName":"private-host","name":"private-name"}]}),
            "cdi netDevices",
        ),
        (json!({"intelRdt":{"closID":"private-rdt"}}), "cdi intelRdt"),
    ] {
        let parsed = one(data);
        let edits = parsed.select(&["test.example/gpu=one"], 1).unwrap();
        let mut spec = spec();
        spec.init.args.clear();
        assert!(
            matches!(render_with_devices(&spec,&place(),&edits,&[]),Err(Refusal::Platform {what,..}) if what == field)
        );
    }
}

/// CLAUDE §§1–3 / AC-T7.3.1: added input/operation budgets and escaping-expanded JSON use the
/// existing runtime read bound. The writer accepts the exact bound, refuses one byte beyond it,
/// and returns no partially encoded Bundle.
#[test]
fn added_input_and_encoded_output_are_bounded_with_exact_writer_boundary() {
    let huge = "x".repeat(MAX_CONFIG_BYTES);
    let edits = cdi::Edits {
        env: vec![&huge],
        ..Default::default()
    };
    assert!(matches!(
        render_with_devices(&spec(), &place(), &edits, &[]),
        Err(Refusal::Exhausted {
            bound: MAX_CONFIG_BYTES,
            ..
        })
    ));
    let many = cdi::Edits {
        additional_gids: vec![0; MAX_CONFIG_BYTES + 1],
        ..Default::default()
    };
    assert!(matches!(
        render_with_devices(&spec(), &place(), &many, &[]),
        Err(Refusal::Exhausted { .. })
    ));
    let escaped = format!("PRIVATE={}", "\n".repeat(MAX_CONFIG_BYTES / 2));
    let edits = cdi::Edits {
        env: vec![&escaped],
        ..Default::default()
    };
    assert!(matches!(
        render_with_devices(&spec(), &place(), &edits, &[]),
        Err(Refusal::Invalid {
            what: "oci spec",
            ..
        })
    ));
    let mut out = Output {
        bytes: Vec::new(),
        failure: None,
    };
    out.write_all(&vec![b'x'; MAX_CONFIG_BYTES]).unwrap();
    assert_eq!(out.bytes.len(), MAX_CONFIG_BYTES);
    assert!(out.write_all(b"!").is_err());
    assert_eq!(out.bytes.len(), MAX_CONFIG_BYTES);
    assert!(matches!(out.failure, Some(Cause::Limit)));
}

/// CLAUDE §3 / R04: errors/resolution Debug contain field/cause only, never private paths/env.
#[test]
fn errors_and_resolution_debug_do_not_render_private_content() {
    let node: cdi::DeviceNode =
        serde_json::from_value(json!({"path":"/private-node","hostPath":"/private-host"})).unwrap();
    let other: cdi::DeviceNode = serde_json::from_value(json!({"path":"/private-other"})).unwrap();
    let edits = cdi::Edits {
        device_nodes: vec![&node],
        env: vec!["PRIVATE_TOKEN=hidden"],
        ..Default::default()
    };
    let receipt = receipt(&other, DeviceType::Char, 1, 3);
    let err = render_with_devices(&spec(), &place(), &edits, &[receipt]).unwrap_err();
    let text = format!(
        "{err:?} {err} {:?}",
        super::tests::receipt(&node, DeviceType::Char, 1, 3)
    );
    for private in [
        "private-node",
        "private-host",
        "private-other",
        "PRIVATE_TOKEN",
        "hidden",
    ] {
        assert!(!text.contains(private), "{text}");
    }
}
