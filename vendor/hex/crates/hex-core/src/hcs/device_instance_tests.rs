use super::*;

const GUID: &str = "5B45201D-F2F2-4F3B-85BB-30FF1F953599";
const PRIVATE_MARKER: &str = "HEX_HCS_SYNTHETIC_PRIVATE_DEVICE_VALUE";

fn bundle(devices: Value) -> Value {
    json!({
        "ociVersion": "1.2.0",
        "process": {"args": ["cmd.exe"]},
        "windows": {"layerFolders": ["D:\\layers\\base", "D:\\scratch"], "devices": devices}
    })
}

fn render(container: &Container) -> Value {
    document(
        container,
        &Storage {
            volume: r"\\?\Volume{11111111-1111-1111-1111-111111111111}".to_owned(),
            layers: vec![("layer".to_owned(), r"D:\layers\base".to_owned())],
        },
        r"C:\launcher",
        Lifetime::EndsWithNode,
    )
}

fn refusal_names_only_field(devices: Value, field: &str) {
    let refusal = from_bundle(&bundle(devices)).unwrap_err();
    assert!(matches!(
        &refusal,
        Refusal::Invalid {
            what: "oci spec",
            ..
        }
    ));
    for text in [refusal.to_string(), format!("{refusal:?}")] {
        assert!(text.contains(field));
        assert!(
            !text.contains(PRIVATE_MARKER),
            "device value leaked into refusal"
        );
    }
}

/// AC-T7.4.1 / HEX-HCS-3,6: a precise OCI assignment stays exact and selects the first HCS schema
/// supporting DeviceInstance/LocationPath, independently of any actual hardware acceptance.
#[test]
fn location_path_renders_only_device_instance_with_minimum_schema() {
    let path = r"PCIROOT(0)#PCI(0100)#PCI(0000)";
    let container =
        from_bundle(&bundle(json!([{"idType":"vpci-location-path","id":path}]))).unwrap();
    assert_eq!(
        container.devices,
        vec![AssignedDevice::LocationPath(path.to_owned())]
    );
    let document = render(&container);
    assert_eq!(document["SchemaVersion"], json!({"Major":2,"Minor":2}));
    assert_eq!(
        document["Container"]["AssignedDevices"],
        json!([
            {"Type":"DeviceInstance","LocationPath":path}
        ])
    );
}

/// HEX-HCS-3: the adapter adds no path grammar, normalization, trimming or case folding.
#[test]
fn location_path_is_an_opaque_exact_string() {
    for path in [" opaque / ../ pCi(00) ", "opaque-λ\n#\"\\value", " "] {
        let container =
            from_bundle(&bundle(json!([{"idType":"vpci-location-path","id":path}]))).unwrap();
        assert_eq!(
            container.devices,
            vec![AssignedDevice::LocationPath(path.to_owned())]
        );
        assert_eq!(
            render(&container)["Container"]["AssignedDevices"],
            json!([
                {"Type":"DeviceInstance","LocationPath":path}
            ])
        );
    }
}

/// HEX-HCS-3,6: both published class idTypes retain normalization and the entire established
/// class-only document, including schema 2.1. DeviceInstance support does not widen this grant.
#[test]
fn legacy_class_guid_inputs_keep_the_existing_document() {
    let expected = json!({
        "Owner":"hex",
        "SchemaVersion":{"Major":2,"Minor":1},
        "ShouldTerminateOnLastHandleClosed":true,
        "Container":{
            "Storage":{
                "Path":r"\\?\Volume{11111111-1111-1111-1111-111111111111}\",
                "Layers":[{"Id":"layer","Path":r"D:\layers\base"}]
            },
            "MappedDirectories":[{"HostPath":r"C:\launcher","ContainerPath":r"C:\hex","ReadOnly":true}],
            "AssignedDevices":[{"Type":"ClassGuid","InterfaceClassGuid":GUID}]
        }
    });
    for kind in ["class", "vpci-class-guid"] {
        let container = from_bundle(&bundle(json!([{
            "idType":kind,"id":"{5b45201d-f2f2-4f3b-85bb-30ff1f953599}"
        }])))
        .unwrap();
        assert_eq!(
            container.devices,
            vec![AssignedDevice::ClassGuid(GUID.to_owned())]
        );
        assert_eq!(render(&container), expected);
    }
}

/// AC-T7.4.1 / HEX-HCS-6: mixed caller order is retained without substituting an interface class
/// for an instance, dropping either assignment, or selecting GpuMirror.
#[test]
fn mixed_assignments_keep_types_values_and_order() {
    let container = from_bundle(&bundle(json!([
        {"idType":"vpci-location-path","id":"exact-first"},
        {"idType":"class","id":GUID},
        {"idType":"vpci-location-path","id":"exact-last"},
        {"idType":"vpci-class-guid","id":GUID}
    ])))
    .unwrap();
    assert_eq!(
        container.devices,
        vec![
            AssignedDevice::LocationPath("exact-first".to_owned()),
            AssignedDevice::ClassGuid(GUID.to_owned()),
            AssignedDevice::LocationPath("exact-last".to_owned()),
            AssignedDevice::ClassGuid(GUID.to_owned()),
        ]
    );
    let document = render(&container);
    assert_eq!(document["SchemaVersion"], json!({"Major":2,"Minor":2}));
    assert_eq!(
        document["Container"]["AssignedDevices"],
        json!([
            {"Type":"DeviceInstance","LocationPath":"exact-first"},
            {"Type":"ClassGuid","InterfaceClassGuid":GUID},
            {"Type":"DeviceInstance","LocationPath":"exact-last"},
            {"Type":"ClassGuid","InterfaceClassGuid":GUID}
        ])
    );
}

/// HEX-HCS-6: an absent or empty list still emits no assignment and keeps schema 2.1.
#[test]
fn empty_and_absent_devices_keep_the_ordinary_schema() {
    let empty = bundle(json!([]));
    let mut absent = empty.clone();
    absent["windows"].as_object_mut().unwrap().remove("devices");
    for config in [empty, absent] {
        let container = from_bundle(&config).unwrap();
        assert!(container.devices.is_empty());
        let document = render(&container);
        assert_eq!(document["SchemaVersion"], json!({"Major":2,"Minor":1}));
        assert!(document["Container"].get("AssignedDevices").is_none());
    }
}

/// HEX-HCS-4: other extension names and malformed kinds are typed field refusals, with neither
/// the idType nor device id copied into their Display/Debug. All markers here are synthetic.
#[test]
fn unsupported_or_malformed_id_types_refuse_without_private_values() {
    for kind in [
        json!("gpu"),
        json!("vpci"),
        json!("vpci-instance-id"),
        json!("DeviceInstance"),
        json!("GpuMirror"),
        json!(PRIVATE_MARKER),
        json!(""),
        json!(null),
        json!(false),
        json!(format!("vpci-location-path\0{PRIVATE_MARKER}")),
    ] {
        refusal_names_only_field(
            json!([{"idType":kind,"id":PRIVATE_MARKER}]),
            "windows.devices[0].idType",
        );
    }
    refusal_names_only_field(json!([{"id":PRIVATE_MARKER}]), "windows.devices[0].idType");
}

/// HEX-HCS-4: missing/non-string/empty/NUL location identifiers are rejected by field and cause.
#[test]
fn invalid_location_identifiers_refuse_without_private_values() {
    for id in [
        json!(null),
        json!(3),
        json!(""),
        json!(format!("{PRIVATE_MARKER}\0tail")),
    ] {
        refusal_names_only_field(
            json!([{"idType":"vpci-location-path","id":id}]),
            "windows.devices[0].id",
        );
    }
    refusal_names_only_field(
        json!([{"idType":"vpci-location-path"}]),
        "windows.devices[0].id",
    );
}

/// HEX-HCS-3,4 / CLAUDE.md §2: reuse the existing config byte bound before copying an opaque
/// caller value. This direct-Value boundary does not claim its whole JSON fits the file reader.
#[test]
fn location_identifier_copy_uses_the_existing_config_byte_budget() {
    let exact = "x".repeat(crate::bundle::MAX_CONFIG_BYTES);
    let container =
        from_bundle(&bundle(json!([{"idType":"vpci-location-path","id":exact}]))).unwrap();
    assert_eq!(container.devices, vec![AssignedDevice::LocationPath(exact)]);
    let oversized = format!(
        "{PRIVATE_MARKER}{}",
        "x".repeat(crate::bundle::MAX_CONFIG_BYTES)
    );
    refusal_names_only_field(
        json!([{"idType":"vpci-location-path","id":oversized}]),
        "windows.devices[0].id",
    );
}

/// HEX-HCS-4: existing class validation also reports only field/cause when its identifier is bad.
#[test]
fn invalid_class_guid_diagnostics_do_not_echo_the_identifier() {
    for kind in ["class", "vpci-class-guid"] {
        refusal_names_only_field(
            json!([{"idType":kind,"id":PRIVATE_MARKER}]),
            "windows.devices[0].id",
        );
    }
}
