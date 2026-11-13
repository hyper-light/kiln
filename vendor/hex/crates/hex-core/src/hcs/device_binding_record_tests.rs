//! AC-T7.4.1/T-T7.1.2: durable exact native binding data; these are pure marker codecs, not HCS effects.
use super::*;
use hex_api::{BoxId, DeviceId, DeviceKind};

fn record() -> BoxRecord {
    BoxRecord {
        storage: RecordStorage::OwnedFolder,
        creation: NativeCreate::Created,
        folder: r"C:\owned\scratch".to_owned(),
        made: Vec::new(),
        outlives: false,
        system: None,
        started: false,
        init: None,
        devices: Vec::new(),
    }
}

fn binding() -> DeviceBinding {
    DeviceBinding {
        id: DeviceId {
            kind: DeviceKind::Gpu,
            key: 0x1234_5678_abcd,
        },
        location: "synthetic-private-PCIROOT(0)#PCI(0200)".to_owned(),
    }
}

#[test]
fn legacy_empty_binding_metadata_keeps_the_exact_existing_json() {
    let old = br#"{"folder":"C:\\owned\\scratch","made":[],"outlives":false,"system":null,"started":false,"init":null}"#;
    let decoded = read_record(old).unwrap();
    assert!(decoded.devices.is_empty());
    assert_eq!(write_record(&decoded).unwrap(), old);
    assert_eq!(decoded, record());
}

#[test]
fn exact_id_and_opaque_location_are_retained_in_the_same_marker() {
    let mut expected = record();
    expected.devices.push(binding());
    expected.devices[0].location.push_str("\\é\"opaque");
    let bytes = write_record(&expected).unwrap();
    let decoded = read_record(&bytes).unwrap();
    assert_eq!(decoded, expected);
    assert_eq!(write_record(&decoded).unwrap(), bytes);
}

#[test]
fn malformed_binding_identity_or_location_refuses_without_private_content() {
    let mut bad = binding();
    bad.id.kind = DeviceKind::Net;
    bad.id.key = u64::MAX;
    let mut nul = binding();
    nul.location.push('\0');
    let mut empty = binding();
    empty.location.clear();
    for bindings in [
        vec![bad],
        vec![nul],
        vec![empty],
        vec![binding(), binding()],
    ] {
        let mut input = record();
        input.devices = bindings;
        let error = write_record(&input).unwrap_err();
        assert!(matches!(error, Refusal::Invalid { .. }));
        assert!(!error.to_string().contains("synthetic-private"));
        let raw = serde_json::to_vec(&input).unwrap();
        let error = read_record(&raw).unwrap_err();
        assert!(matches!(
            error,
            Refusal::Invalid {
                what: "box record",
                ..
            }
        ));
        assert!(!error.to_string().contains("synthetic-private"));
    }
}

#[test]
fn the_combined_marker_bound_is_derived_and_each_field_keeps_its_own_budget() {
    let path = "\u{1}".repeat(FOLDER_UNITS);
    let mut maximum = BoxRecord {
        storage: RecordStorage::OwnedFolder,
        creation: NativeCreate::Created,
        folder: path.clone(),
        made: vec![path.clone(), path],
        outlives: false,
        system: Some(BoxId::new(&"x".repeat(BoxId::MAX_LEN)).unwrap()),
        started: false,
        init: Some(InitRecord {
            pid: u32::MAX,
            created: u64::MAX,
        }),
        devices: Vec::new(),
    };
    assert_eq!(write_record(&maximum).unwrap().len(), MAX_RECORD);
    maximum.devices.push(binding());
    maximum.devices[0].location = "x".to_owned();
    let overhead = serde_json::to_vec(&maximum.devices).unwrap().len() + b",\"devices\":".len() - 1;
    maximum.devices[0].location = "x".repeat(crate::bundle::MAX_CONFIG_BYTES - overhead);
    let bytes = write_record(&maximum).unwrap();
    assert_eq!(
        record_bound().unwrap(),
        MAX_RECORD
            .checked_add(crate::bundle::MAX_CONFIG_BYTES)
            .unwrap()
    );
    assert_eq!(bytes.len(), record_bound().unwrap());
    assert_eq!(read_record(&bytes).unwrap(), maximum);
    maximum.devices[0].location.push('x');
    assert!(write_record(&maximum).is_err());
    maximum.devices[0].location.pop();
    maximum.folder.push('\u{1}');
    assert!(write_record(&maximum).is_err());
}

#[test]
fn an_input_past_the_combined_ceiling_is_refused_before_json_parsing() {
    let bytes = vec![b' '; record_bound().unwrap() + 1];
    assert!(matches!(
        read_record(&bytes),
        Err(Refusal::Invalid {
            what: "box record",
            ..
        })
    ));
    let error = read_record(br#"{"devices":"synthetic-private-payload"}"#).unwrap_err();
    assert!(!error.to_string().contains("synthetic-private"));
}

#[test]
fn borrowed_volume_ownership_and_pending_create_roundtrip_without_changing_legacy_json() {
    let mut input = record();
    input.folder = r"\\?\Volume{ec84d99e-3f02-11e7-ac6c-00155d7682cf}\".to_owned();
    input.storage = RecordStorage::BorrowedVolume;
    input.creation = NativeCreate::Pending;
    input.system = Some(BoxId::new("owned-pending").unwrap());
    input.devices.push(binding());
    let bytes = write_record(&input).unwrap();
    assert_eq!(read_record(&bytes).unwrap(), input);
    assert!(
        String::from_utf8(bytes.clone())
            .unwrap()
            .contains("\"storage\":\"borrowed_volume\"")
    );
    assert!(
        String::from_utf8(bytes)
            .unwrap()
            .contains("\"creation\":\"pending\"")
    );
    input.made.push("synthetic-private-unowned-path".to_owned());
    let error = write_record(&input).unwrap_err();
    assert!(matches!(
        error,
        Refusal::Invalid {
            what: "box record storage",
            ..
        }
    ));
    assert!(!error.to_string().contains("synthetic-private"));
    let raw = serde_json::to_vec(&input).unwrap();
    assert!(read_record(&raw).is_err());
    input.made.clear();
    input.system = None;
    assert!(matches!(
        write_record(&input),
        Err(Refusal::Invalid {
            what: "box record creation",
            ..
        })
    ));
}

#[test]
fn new_storage_and_creation_fields_share_the_existing_derived_metadata_budget() {
    let mut input = record();
    input.devices.push(binding());
    input.storage = RecordStorage::BorrowedVolume;
    input.creation = NativeCreate::Pending;
    input.system = Some(BoxId::new("pending-budget").unwrap());
    let fixed = record_extra(&input).unwrap() - input.devices[0].location.len();
    input.devices[0].location = "x".repeat(crate::bundle::MAX_CONFIG_BYTES - fixed);
    assert_eq!(
        record_extra(&input).unwrap(),
        crate::bundle::MAX_CONFIG_BYTES
    );
    assert_eq!(read_record(&write_record(&input).unwrap()).unwrap(), input);
    input.devices[0].location.push('x');
    assert!(write_record(&input).is_err());
    for field in ["storage", "creation"] {
        let mut bad = serde_json::to_value(record()).unwrap();
        bad[field] = serde_json::json!("synthetic-private-unknown-kind");
        let error = read_record(&serde_json::to_vec(&bad).unwrap()).unwrap_err();
        assert!(matches!(
            error,
            Refusal::Invalid {
                what: "box record",
                ..
            }
        ));
        assert!(!error.to_string().contains("synthetic-private"));
    }
}

/// AC-T7.4.1: the portable key is u64; marker codecs retain it without claiming native authority.
#[test]
fn full_native_gpu_keys_roundtrip_without_pci_narrowing() {
    for key in [1 << 48, u64::MAX] {
        let mut input = record();
        let mut device = binding();
        device.id.key = key;
        input.devices.push(device);
        let decoded = read_record(&write_record(&input).unwrap()).unwrap();
        assert_eq!(decoded.devices[0].id.key, key);
        assert_eq!(decoded, input);
    }
}
