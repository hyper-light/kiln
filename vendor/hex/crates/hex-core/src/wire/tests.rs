//! The wire format's tests (decision 3, HEX-SOCK-1 and HEX-SOCK-2): every request and answer reads
//! back as written; a message of another wire is refused before its body is read, naming both wires, and a
//! hello's head names its sender's build whatever the wire; the wire's identity is its definitions' digest,
//! line endings aside; and hostile bytes, at every truncation, every bit flip and at random, are refused by
//! type and never panic.

use super::*;
use hex_api::{
    Bounds, BoxOwner, BoxSpec, IoRates, Mount, Network, OciSpec, Program, Rootfs, StartedFrom, User,
};
use proptest::prelude::*;

fn device_claims() -> Vec<DeviceId> {
    vec![
        DeviceId {
            kind: DeviceKind::Net,
            key: u64::from(u32::MAX),
        },
        DeviceId {
            kind: DeviceKind::Gpu,
            key: 0x1234_5678_abcd,
        },
        DeviceId {
            kind: DeviceKind::Block,
            key: u64::MAX,
        },
        DeviceId {
            kind: DeviceKind::Vfio,
            key: 0,
        },
    ]
}

fn device_spec(devices: Vec<DeviceId>) -> BoxSpec {
    BoxSpec {
        allocation: None,
        route: None,
        id: id("device-wire"),
        bounds: Bounds::default(),
        class: ClassSpec::Process,
        owner: None,
        devices,
        band: 0,
    }
}

fn device_capacity(devices: Vec<DeviceInfo>) -> Capacity {
    Capacity {
        own: Default::default(),
        cost: None,
        routes: Vec::new(),
        device_routes: Vec::new(),
        backings: Vec::new(),
        id: "physical-pool".to_owned(),
        total: CapacityResources::default(),
        free: CapacityResources::default(),
        devices,
        unavailable: Vec::new(),
        learned: None,
        node_vms: Vec::new(),
        unlearned: None,
    }
}

fn device_offer(device: DeviceId) -> DeviceInfo {
    DeviceInfo {
        id: device,
        model: "synthetic-private-model-é".to_owned(),
        size: u64::MAX,
        holder: Some(DeviceHolder::Box(id("synthetic-private-holder"))),
    }
}

// Keep all preceding fields exactly as the real codec writes them; replace only the device tail.
fn with_device_tail(offer: bool, tail: &[u8]) -> Vec<u8> {
    let mut bytes = if offer {
        answer(
            91,
            &Answer::Capacity(Some(device_capacity(Vec::new()))),
            MAX_MESSAGE,
        )
        .unwrap()
    } else {
        request(
            91,
            &Request::Call(Call::Create(Box::new(device_spec(Vec::new())))),
            MAX_MESSAGE,
        )
        .unwrap()
    };
    // Replace the device list while retaining valid absent charge/route receipts. Capacity
    // additionally ends with four empty u32 lists, no learned use and its machine's time unread (joint plan step 14,
    // C7), no node VM's receipt (joint plan step 39), no learner's why (A-3) and its boxes' CPU time unread (A-8); a spec
    // ends with one route option and its band, 0 here (joint plan step 14).
    let following = if offer {
        1 + 4 * size_of::<u32>() + 1 + 2 + size_of::<u32>() + 1 + 1
    } else {
        3
    };
    bytes.truncate(bytes.len() - size_of::<u32>() - following);
    bytes.extend_from_slice(tail);
    bytes.extend(std::iter::repeat_n(0, following));
    bytes
}

fn read_device_tail(offer: bool, tail: &[u8]) -> Fault {
    let bytes = with_device_tail(offer, tail);
    if offer {
        read_answer(&received(&bytes, 0), MAX_MESSAGE).unwrap_err()
    } else {
        read_request(&received(&bytes, 0), MAX_MESSAGE)
            .unwrap_err()
            .fault
    }
}

/// AC-T7.3.1/HEX-SOCK-1: fixed device tags and LE keys; catalog/claim order and shared kinds survive.
#[test]
fn device_claims_and_offers_roundtrip_with_explicit_tags_and_catalog_order() {
    let devices = device_claims();
    let mut encoded = Writer::new(MAX_MESSAGE);
    for device in &devices {
        put_device_id(&mut encoded, device);
    }
    let mut expected = Vec::new();
    for (tag, key) in [
        (3, u64::from(u32::MAX)),
        (0, 0x1234_5678_abcd),
        (2, u64::MAX),
        (1, 0),
    ] {
        expected.push(tag);
        expected.extend_from_slice(&key.to_le_bytes());
    }
    assert_eq!(encoded.done().unwrap(), expected);
    let claimed = Request::Call(Call::Create(Box::new(device_spec(devices.clone()))));
    let bytes = request(91, &claimed, MAX_MESSAGE).unwrap();
    assert_eq!(&bytes[12..14], &kind::CREATE.to_le_bytes());
    assert_eq!(
        read_request(&received(&bytes, 0), MAX_MESSAGE).unwrap().1,
        claimed
    );
    assert_eq!(
        request(
            91,
            &read_request(&received(&bytes, 0), MAX_MESSAGE).unwrap().1,
            MAX_MESSAGE
        )
        .unwrap(),
        bytes
    );
    let mut offers: Vec<_> = devices.into_iter().map(device_offer).collect();
    offers[1].holder = None;
    offers[2].model.clear(); // Opaque metadata has no invented naming grammar.
    let offered = Answer::Capacity(Some(device_capacity(offers)));
    let bytes = answer(91, &offered, MAX_MESSAGE).unwrap();
    assert_eq!(&bytes[12..14], &kind::CAPACITY_ANSWER.to_le_bytes());
    let decoded = read_answer(&received(&bytes, 0), MAX_MESSAGE).unwrap().1;
    assert_eq!(decoded, offered);
    assert_eq!(answer(91, &decoded, MAX_MESSAGE).unwrap(), bytes);
}

/// AC-T7.4.1: a capacity's devices it lists but won't offer, each with why, cross the wire after its routes, before
/// only what the node learned (joint plan step 14) and its node VMs (joint plan step 39). Do: encode a capacity listing
/// one of its two devices unavailable; cut the same capacity, listing none, no learned use and no node VM, before that
/// list's count; read an
/// older JSON report, which has no such field; and encode one listing a device it doesn't hold. Expect the first back
/// exactly, the cut refused as truncated, as every truncation is (only this same wire's builds talk: IDENTITY), the
/// JSON report listing none, and the last refused by the capacity's own check before any byte is written.
#[test]
fn a_capacitys_unavailable_devices_cross_the_wire_last() {
    let devices = vec![
        DeviceInfo {
            id: DeviceId {
                kind: DeviceKind::Vfio,
                key: 7,
            },
            model: "group-7".into(),
            size: 0,
            holder: None,
        },
        DeviceInfo {
            id: DeviceId {
                kind: DeviceKind::Vfio,
                key: 8,
            },
            model: "group-8".into(),
            size: 0,
            holder: None,
        },
    ];
    let mut listed = device_capacity(devices);
    listed.unavailable = vec![hex_api::DeviceUnavailable {
        id: listed.devices[0].id,
        why: "0000:01:00.0 has no reset method that clears it between VMs; the host must reboot"
            .into(),
    }];
    let offered = Answer::Capacity(Some(listed.try_clone().unwrap()));
    let bytes = answer(91, &offered, MAX_MESSAGE).unwrap();
    assert_eq!(
        read_answer(&received(&bytes, 0), MAX_MESSAGE).unwrap().1,
        offered
    );
    let mut none = listed.try_clone().unwrap();
    none.unavailable.clear();
    let mut cut = answer(
        91,
        &Answer::Capacity(Some(none.try_clone().unwrap())),
        MAX_MESSAGE,
    )
    .unwrap();
    assert_eq!(
        cut[cut.len() - 13..],
        [0; 13],
        "the empty list's count, then no learned use, its machine's time unread, no node VM's count, no learner's why \
         and its boxes' CPU time unread, ends it"
    );
    cut.truncate(cut.len() - 13);
    assert!(matches!(
        read_answer(&received(&cut, 0), MAX_MESSAGE),
        Err(Fault::Truncated { .. })
    ));
    let older = serde_json::to_value(&none).unwrap();
    assert!(older.get("unavailable").is_none(), "skipped when empty");
    let read: Capacity = serde_json::from_value(older).unwrap();
    assert_eq!(read, none);
    let mut stray = listed;
    stray.unavailable[0].id.key = 9;
    assert!(answer(91, &Answer::Capacity(Some(stray)), MAX_MESSAGE).is_err());
}

/// AC-T7.4.1: a native Metal registry identity uses all64 bits; the wire does not reinterpret
/// it as a narrow PCI address. The initialized adapter validates the physical correspondence.
#[test]
fn native_gpu_keys_roundtrip_without_pci_width_assumptions() {
    for key in [1 << 48, 1 << 63, u64::MAX] {
        let gpu = DeviceId {
            kind: DeviceKind::Gpu,
            key,
        };
        let claimed = Request::Call(Call::Create(Box::new(device_spec(vec![gpu]))));
        let bytes = request(91, &claimed, MAX_MESSAGE).unwrap();
        assert_eq!(
            read_request(&received(&bytes, 0), MAX_MESSAGE).unwrap().1,
            claimed
        );
        let offered = Answer::Capacity(Some(device_capacity(vec![device_offer(gpu)])));
        let bytes = answer(91, &offered, MAX_MESSAGE).unwrap();
        assert_eq!(
            read_answer(&received(&bytes, 0), MAX_MESSAGE).unwrap().1,
            offered
        );
    }
}

/// AC-T7.3.1: malformed claims and offers refuse on both sides, rather than narrowing an identity.
#[test]
fn device_codecs_refuse_invalid_keys_and_duplicate_ids_without_private_values() {
    for device in [
        DeviceId {
            kind: DeviceKind::Vfio,
            key: 1 << 32,
        },
        DeviceId {
            kind: DeviceKind::Net,
            key: 0,
        },
        DeviceId {
            kind: DeviceKind::Net,
            key: 1 << 32,
        },
    ] {
        let claimed = Request::Call(Call::Create(Box::new(device_spec(vec![device]))));
        assert!(matches!(
            request(91, &claimed, MAX_MESSAGE),
            Err(Fault::Value {
                what: "devices",
                ..
            })
        ));
        let offered = Answer::Capacity(Some(device_capacity(vec![device_offer(device)])));
        assert!(matches!(
            answer(91, &offered, MAX_MESSAGE),
            Err(Fault::Value {
                what: "capacity",
                ..
            })
        ));
        for offer in [false, true] {
            let mut tail = Writer::new(MAX_MESSAGE);
            tail.u32(1);
            put_device_id(&mut tail, &device);
            if offer {
                tail.string("synthetic-private-model").unwrap();
                tail.u64(1);
                tail.u8(0);
            }
            let error = read_device_tail(offer, &tail.done().unwrap());
            assert!(matches!(
                error,
                Fault::Value {
                    what: "devices.id",
                    ..
                }
            ));
            assert!(!error.to_string().contains("synthetic-private"));
            assert!(!error.to_string().contains(&device.key.to_string()));
        }
    }
    let device = device_claims()[0];
    let claimed = Request::Call(Call::Create(Box::new(device_spec(vec![device, device]))));
    assert!(request(91, &claimed, MAX_MESSAGE).is_err());
    let offered = Answer::Capacity(Some(device_capacity(vec![
        device_offer(device),
        device_offer(device),
    ])));
    assert!(answer(91, &offered, MAX_MESSAGE).is_err());
    for offer in [false, true] {
        let mut tail = Writer::new(MAX_MESSAGE);
        tail.u32(2);
        for _ in 0..2 {
            put_device_id(&mut tail, &device);
            if offer {
                tail.string("synthetic-private-model").unwrap();
                tail.u64(1);
                tail.u8(0);
            }
        }
        let error = read_device_tail(offer, &tail.done().unwrap());
        assert!(matches!(error, Fault::Value { ref why, .. } if why.contains("duplicate")));
        assert!(!error.to_string().contains("synthetic-private"));
    }
}

/// AC-T7.3.1/HEX-SOCK-2: reject hostile counts before reserving or visiting list elements.
#[test]
fn device_lists_refuse_oversized_and_truncated_counts_before_visiting_elements() {
    for offer in [false, true] {
        for count in [u32::try_from(MAX_DEVICES + 1).unwrap(), u32::MAX] {
            assert!(matches!(
                read_device_tail(offer, &count.to_le_bytes()),
                Fault::Value {
                    what: "devices",
                    ..
                }
            ));
            let bytes = count.to_le_bytes();
            let mut reader = Reader::new(&bytes);
            let mut visits = 0;
            assert!(
                reader
                    .device_list::<DeviceId>(if offer { 22 } else { 9 }, |_| {
                        visits += 1;
                        panic!("oversized count visited an element")
                    })
                    .is_err()
            );
            assert_eq!(visits, 0);
            assert_eq!(reader.at(), 4);
        }
        for count in [1, u32::try_from(MAX_DEVICES).unwrap()] {
            assert!(matches!(
                read_device_tail(offer, &count.to_le_bytes()),
                Fault::Truncated { .. }
            ));
        }
        let oversized: Vec<_> = (0..=MAX_DEVICES)
            .map(|key| DeviceId {
                kind: DeviceKind::Block,
                key: key as u64,
            })
            .collect();
        let error = if offer {
            answer(
                91,
                &Answer::Capacity(Some(device_capacity(
                    oversized.into_iter().map(device_offer).collect(),
                ))),
                MAX_MESSAGE,
            )
            .unwrap_err()
        } else {
            request(
                91,
                &Request::Call(Call::Create(Box::new(device_spec(oversized)))),
                MAX_MESSAGE,
            )
            .unwrap_err()
        };
        assert!(matches!(error, Fault::Value { .. }));
    }
    // A valid API list can exceed this transport's independent byte bound; the writer refuses early.
    let claimed = Request::Call(Call::Create(Box::new(device_spec(
        (0..MAX_DEVICES)
            .map(|key| DeviceId {
                kind: DeviceKind::Block,
                key: key as u64,
            })
            .collect(),
    ))));
    assert!(matches!(
        request(91, &claimed, MAX_MESSAGE),
        Err(Fault::Oversized {
            bound: MAX_MESSAGE,
            ..
        })
    ));
}

/// AC-T7.3.1/HEX-SOCK-2: unknown tags/invalid holder data are refusals, never default grants or echoes.
#[test]
fn device_unknown_tags_and_invalid_metadata_diagnostics_are_redacted() {
    for offer in [false, true] {
        for tag in [4, u8::MAX] {
            let mut tail = Writer::new(MAX_MESSAGE);
            tail.u32(1);
            tail.u8(tag);
            tail.u64(1);
            if offer {
                tail.string("synthetic-private-model").unwrap();
                tail.u64(1);
                tail.u8(0);
            }
            let error = read_device_tail(offer, &tail.done().unwrap());
            assert!(matches!(
                error,
                Fault::Value {
                    what: "devices.kind",
                    ..
                }
            ));
            assert!(!error.to_string().contains("synthetic-private"));
        }
    }
    let device = device_claims()[0];
    let mut tail = Writer::new(MAX_MESSAGE);
    tail.u32(1);
    put_device_id(&mut tail, &device);
    tail.string("synthetic-private-model").unwrap();
    tail.u64(1);
    tail.u8(1);
    tail.string("../synthetic-private-holder").unwrap();
    let error = read_device_tail(true, &tail.done().unwrap());
    assert!(matches!(
        error,
        Fault::Value {
            what: "devices.holder",
            ..
        }
    ));
    assert!(!error.to_string().contains("synthetic-private"));
    let mut tail = Writer::new(MAX_MESSAGE);
    tail.u32(1);
    put_device_id(&mut tail, &device);
    tail.bytes(&[0xff]).unwrap();
    tail.u64(1);
    tail.u8(0);
    assert!(matches!(
        read_device_tail(true, &tail.done().unwrap()),
        Fault::Value {
            what: "devices.model",
            ..
        }
    ));
}

/// AC-T7.4.1: capacity travels as a typed call/answer, including native host fallback.
#[test]
fn runtime_capacity_roundtrip_and_invalid_room_refused() {
    let request = Request::Call(Call::Capacity(Class::Oci));
    let bytes = super::request(42, &request, MAX_MESSAGE).unwrap();
    assert_eq!(
        read_request(&received(&bytes, 0), MAX_MESSAGE).unwrap().1,
        request
    );
    let capacity = hex_api::Capacity {
        // Its machine's time and its boxes' together (joint plan step 14, C7; the review's A-8) cross with it.
        own: hex_api::MachineTime {
            cpu_ns: Some(4_000_000_000),
            uptime_ns: Some(10_000_000_000),
            boxes_cpu_ns: Some(1_000_000_000),
        },
        cost: None,
        routes: Vec::new(),
        device_routes: Vec::new(),
        backings: Vec::new(),
        devices: Vec::new(),
        id: "guest:123:456".into(),
        total: hex_api::CapacityResources {
            cpu_milli: 2_000,
            mem_bytes: 632 << 20,
            disk_bytes: 8 << 30,
        },
        free: hex_api::CapacityResources {
            cpu_milli: 2_000,
            mem_bytes: 600 << 20,
            disk_bytes: 7 << 30,
        },
        unavailable: Vec::new(),
        learned: None,
        node_vms: Vec::new(),
        unlearned: None,
    };
    for value in [None, Some(capacity.try_clone().unwrap())] {
        let expected = Answer::Capacity(value);
        let bytes = answer(42, &expected, MAX_MESSAGE).unwrap();
        assert_eq!(
            read_answer(&received(&bytes, 0), MAX_MESSAGE).unwrap().1,
            expected
        );
    }
    let mut malformed = answer(
        42,
        &Answer::Capacity(Some(capacity.try_clone().unwrap())),
        MAX_MESSAGE,
    )
    .unwrap();
    // Header, Some tag, pool string, total resources and free CPU precede free memory.
    let free_memory =
        HEADER_BYTES + 1 + size_of::<u32>() + capacity.id.len() + RESOURCE_BYTES + size_of::<u64>();
    malformed[free_memory..free_memory + 8]
        .copy_from_slice(&(capacity.total.mem_bytes + 1).to_le_bytes());
    assert!(read_answer(&received(&malformed, 0), MAX_MESSAGE).is_err());
    let mut bad = capacity;
    bad.free.mem_bytes = bad.total.mem_bytes + 1;
    assert!(answer(42, &Answer::Capacity(Some(bad)), MAX_MESSAGE).is_err());
}

/// AC-T7.4.1/HEX-SOCK-1: the chosen backend and exact host claims cross the socket together.
#[test]
fn device_backend_identity_survives_create_capacity_and_inspect() {
    let ClassSpec::Oci(workload) = oci_spec().class else {
        panic!("the fixture is an OCI workload");
    };
    for (class, tag, spec_tag, form) in [
        (Class::Oci, 1, 1, ClassSpec::Oci(workload.clone())),
        (
            Class::Firecracker,
            2,
            3,
            ClassSpec::Firecracker(workload.clone()),
        ),
        (Class::Shards, 3, 4, ClassSpec::Shards(workload.clone())),
    ] {
        let spec = BoxSpec {
            class: form,
            devices: device_claims(),
            ..oci_spec()
        };
        let create = Request::Call(Call::Create(Box::new(spec.clone())));
        let bytes = request(93, &create, MAX_MESSAGE).unwrap();
        let decoded = read_request(&received(&bytes, 0), MAX_MESSAGE).unwrap().1;
        assert_eq!(decoded, create);
        let mut reader = Reader::new(&bytes[HEADER_BYTES..]);
        assert_eq!(box_id(&mut reader).unwrap(), spec.id);
        assert_eq!(super::bounds(&mut reader).unwrap(), spec.bounds);
        assert_eq!(reader.u8().unwrap(), spec_tag);

        let capacity = Request::Call(Call::Capacity(class));
        let bytes = request(94, &capacity, MAX_MESSAGE).unwrap();
        assert_eq!(&bytes[HEADER_BYTES..], &[tag]);
        assert_eq!(
            read_request(&received(&bytes, 0), MAX_MESSAGE).unwrap().1,
            capacity
        );
        let expected = Answer::Info(BoxInfo { class, ..info() });
        let bytes = answer(95, &expected, MAX_MESSAGE).unwrap();
        assert_eq!(
            read_answer(&received(&bytes, 0), MAX_MESSAGE).unwrap().1,
            expected
        );
    }
    for tag in [4, u8::MAX] {
        let mut bytes =
            request(94, &Request::Call(Call::Capacity(Class::Oci)), MAX_MESSAGE).unwrap();
        bytes[HEADER_BYTES] = tag;
        assert!(matches!(
            read_request(&received(&bytes, 0), MAX_MESSAGE)
                .unwrap_err()
                .fault,
            Fault::Value { what: "class", .. }
        ));
    }
}

fn id(s: &str) -> BoxId {
    BoxId::new(s).unwrap()
}

fn bounds() -> Bounds {
    Bounds {
        cpu_weight: Some(100),
        cpu_max_milli: None,
        memory_max: Some(64 << 20),
        memory_high: None,
        io_weight: None,
        io_latency_us: Some(500),
        io_max: Some(IoRates {
            read_bps: Some(1 << 20),
            write_bps: None,
            read_iops: None,
            write_iops: Some(100),
        }),
        pids_max: Some(8),
    }
}

fn program() -> Program {
    Program {
        args: vec!["/usr/bin/true".to_owned(), "x y".to_owned()],
        env: vec![("PATH".to_owned(), "/usr/bin:/bin".to_owned())],
        cwd: Some("/".to_owned()),
        user: Some(User {
            uid: 1000,
            gid: 100,
            groups: vec![10, 20],
        }),
    }
}

fn oci_spec() -> BoxSpec {
    BoxSpec {
        allocation: None,
        route: None,
        devices: Vec::new(),
        id: id("oci-1"),
        bounds: Bounds::default(),
        class: ClassSpec::Oci(Box::new(OciSpec {
            rootfs: Rootfs::Layers(vec![PathBuf::from("/l/base"), PathBuf::from("/l/app")]),
            init: Program::of(&["sleep", "infinity"]),
            mounts: vec![Mount {
                source: PathBuf::from("/data"),
                destination: "/mnt".to_owned(),
                read_only: true,
            }],
            hostname: Some("box".to_owned()),
            network: Network::Private,
            hosts: vec![("db".to_owned(), "10.0.0.2".to_owned())],
        })),
        owner: None,
        band: 0,
    }
}

fn info() -> BoxInfo {
    BoxInfo {
        published: Vec::new(),
        grants: Vec::new(),
        allocation: None,
        route: None,
        membership: None,
        id: id("b-1"),
        class: Class::Oci,
        state: BoxState::Running,
        bounds: bounds(),
        adopted: true,
        exit: Some(Exit::Signal(9)),
        abandoned: 2,
        last_resort: None,
        ended: None,
        control: None,
        control_wake: None,
        credentials: Vec::new(),
    }
}

/// One of each request, with the descriptors each carries.
fn requests() -> Vec<(u64, Request, usize)> {
    let b = id("b-1");
    vec![
        (1, Request::Hello { build: build() }, 0),
        (
            2,
            Request::Call(Call::Create(Box::new(BoxSpec {
                allocation: None,
                route: None,
                devices: Vec::new(),
                id: b.clone(),
                bounds: bounds(),
                class: ClassSpec::Bundle(PathBuf::from("/run/bundle-sleep")),
                owner: None,
                band: 0,
            }))),
            0,
        ),
        (3, Request::Call(Call::Create(Box::new(oci_spec()))), 0),
        (
            4,
            Request::Call(Call::Create(Box::new(BoxSpec {
                allocation: None,
                route: None,
                devices: Vec::new(),
                id: id("p"),
                bounds: Bounds::default(),
                class: ClassSpec::Process,
                owner: None,
                band: 0,
            }))),
            0,
        ),
        (5, Request::Call(Call::Start(b.clone())), 0),
        (
            6,
            Request::Call(Call::Exec {
                id: b.clone(),
                program: program(),
                streams: Streams {
                    stdin: true,
                    stdout: true,
                    stderr: false,
                },
            }),
            2,
        ),
        (
            7,
            Request::Call(Call::Signal {
                id: b.clone(),
                signal: Signal::TERM,
            }),
            0,
        ),
        (8, Request::Call(Call::Inspect(b.clone())), 0),
        (9, Request::Call(Call::Usage(b.clone())), 0),
        (10, Request::Call(Call::List), 0),
        (11, Request::Call(Call::Delete(b)), 0),
        (6, Request::Release, 0),
        (12, Request::Call(Call::Netns), 0),
        (13, Request::Call(Call::Kill { exec: 6 }), 0),
        (14, Request::Taken, 0),
        (16, Request::StreamsReady, 0),
        (
            14,
            Request::Call(Call::ExecStreams {
                id: id("b-1"),
                program: program(),
                streams: Streams {
                    stdin: true,
                    stdout: false,
                    stderr: false,
                },
                ask: Streams {
                    stdin: false,
                    stdout: true,
                    stderr: true,
                },
            }),
            1,
        ),
        (15, run(), 2),
        (87, Request::Call(Call::PhysicalReport), 0),
        (
            201,
            Request::Call(Call::InitializeBacking(triomphe::Arc::new(
                route_wire_grant(),
            ))),
            0,
        ),
        (
            202,
            Request::Call(Call::ReleaseBacking(route_wire_key())),
            0,
        ),
        (203, Request::Call(Call::BackingUsage(route_wire_key())), 0),
        // Joint plan step 17: a member handed over, its streams asked for (a local socket's), and carried (QUIC's).
        (
            204,
            Request::Call(Call::Member {
                id: id("image"),
                member: Member {
                    kind: MemberKind::Harness,
                    name: "claude".to_owned(),
                },
                streams: Streams::default(),
                ask: Streams {
                    stdin: true,
                    stdout: true,
                    stderr: true,
                },
            }),
            0,
        ),
        (
            205,
            Request::Call(Call::Member {
                id: id("image"),
                member: Member {
                    kind: MemberKind::Agent,
                    name: "helper".to_owned(),
                },
                streams: Streams {
                    stdin: true,
                    stdout: true,
                    stderr: true,
                },
                ask: Streams::default(),
            }),
            3,
        ),
        (206, Request::Call(Call::Links), 0),
        // Goal 2's item 2: a member's own signal, appended.
        (
            208,
            Request::Call(Call::MemberSignal {
                id: id("image"),
                member: Member {
                    kind: MemberKind::Harness,
                    name: "claude".to_owned(),
                },
                signal: Signal::TERM,
            }),
            0,
        ),
        // A start followed to its end (`Runtime::started`), appended (ROOT, 2026-11-14).
        (209, Request::Call(Call::Started(id("image"))), 0),
        // The machine's capacity view's word to its node (the review's A-10), appended (ROOT, 2026-11-14).
        (
            210,
            Request::Call(Call::Reserved(hex_api::Reserved {
                keeper: 7,
                reserved: CapacityResources {
                    cpu_milli: 1_500,
                    mem_bytes: 3 << 30,
                    disk_bytes: 9 << 30,
                },
            })),
            0,
        ),
        // Goal 2's item 3: a member's next MCP connection, its stdin carried and its stdout asked for, appended.
        (
            211,
            Request::Call(Call::Mcp {
                id: id("image"),
                member: Member {
                    kind: MemberKind::Agent,
                    name: "helper".to_owned(),
                },
                streams: Streams {
                    stdin: true,
                    stdout: false,
                    stderr: false,
                },
                ask: Streams {
                    stdin: false,
                    stdout: true,
                    stderr: false,
                },
            }),
            1,
        ),
        // docs/design/credential-grants.md C7: a connection's lessors, and its answers to the node's lease asks.
        (205, Request::Call(Call::Lessors), 0),
        (206, Request::Lessor(Ok(())), 0),
        (
            207,
            Request::Lessor(Err(Refusal::Unconfigured {
                what: "lessors for its image VMs' credentials",
                why: "no broker answers this run".to_owned(),
            })),
            0,
        ),
        (
            208,
            Request::Leased {
                run: 206,
                answer: Ok(Lease {
                    lease: "lease-1".to_owned(),
                    value: Value::new(b"a credential's value".to_vec()),
                    life: Some(std::time::Duration::from_secs(90)),
                }),
            },
            0,
        ),
        (
            209,
            Request::Leased {
                run: 206,
                answer: Ok(Lease {
                    lease: "lease-2".to_owned(),
                    value: Value::new(Vec::new()),
                    life: None,
                }),
            },
            0,
        ),
        (
            210,
            Request::Leased {
                run: 206,
                answer: Err(LeaseRefused::Denied("not this run's".to_owned())),
            },
            0,
        ),
        (
            211,
            Request::Leased {
                run: 206,
                answer: Err(LeaseRefused::KeyReplaced {
                    asked: "ab".repeat(32),
                    current: Some("cd".repeat(32)),
                }),
            },
            0,
        ),
        (
            212,
            Request::Leased {
                run: 206,
                answer: Err(LeaseRefused::KeyReplaced {
                    asked: "ab".repeat(32),
                    current: None,
                }),
            },
            0,
        ),
        (
            213,
            Request::Leased {
                run: 206,
                answer: Err(LeaseRefused::Unavailable("its broker is away".to_owned())),
            },
            0,
        ),
        (
            214,
            Request::Leased {
                run: 206,
                answer: Err(LeaseRefused::Ended),
            },
            0,
        ),
        (
            215,
            Request::Leased {
                run: 206,
                answer: Err(LeaseRefused::Lapsed),
            },
            0,
        ),
        // docs/design/credential-grants.md C8: a connection's answers to the node's sign-in asks.
        (
            216,
            Request::Captured {
                run: 206,
                answer: Ok(hex_api::Captured {
                    access: "kiln/sign-ins/claude/access".to_owned(),
                    access_placeholder: "kiln-placeholder-61".to_owned(),
                    refresh_placeholder: Some("kiln-placeholder-62".to_owned()),
                }),
            },
            0,
        ),
        (
            217,
            Request::Captured {
                run: 206,
                answer: Err(SignInRefused::Denied("no such sign-in".to_owned())),
            },
            0,
        ),
        (
            218,
            Request::Refreshed {
                run: 206,
                answer: Ok(Value::new(
                    b"{\"access_token\":\"kiln-placeholder-61\"}".to_vec(),
                )),
            },
            0,
        ),
        (
            219,
            Request::Refreshed {
                run: 206,
                answer: Err(SignInRefused::Unanswered),
            },
            0,
        ),
        (
            220,
            Request::Refreshed {
                run: 206,
                answer: Err(SignInRefused::Unavailable(
                    "its provider is away".to_owned(),
                )),
            },
            0,
        ),
        (
            221,
            Request::Refreshed {
                run: 206,
                answer: Err(SignInRefused::Ended),
            },
            0,
        ),
    ]
}

/// A run (runtime design goal 7) of a `process` box carrying stdout and stderr and asking for stdin.
fn run() -> Request {
    Request::Call(Call::Run {
        spec: Box::new(BoxSpec {
            allocation: None,
            route: None,
            devices: Vec::new(),
            id: id("r"),
            bounds: bounds(),
            class: ClassSpec::Process,
            owner: None,
            band: 0,
        }),
        program: program(),
        streams: Streams {
            stdin: false,
            stdout: true,
            stderr: true,
        },
        ask: Streams {
            stdin: true,
            stdout: false,
            stderr: false,
        },
    })
}

/// The node agent's network namespace counts, each field a different value: makes failed at every step, the pool
/// refused for both capabilities, the latest reason kept at two steps, one with an errno and one without.
fn counts() -> netns::Counts {
    netns::Counts {
        made_ahead: 1,
        taken: 2,
        made_on_path: 3,
        discarded: 4,
        failed: u64::MAX,
        failed_by: netns::FailedBy {
            home: 1,
            unshare: 2,
            open: 3,
            loopback: 4,
            back: 5,
        },
        refused: netns::Missing::of(false, false),
        refused_on_path: 6,
        last_failed: netns::LastFailed {
            unshare: Some(netns::Reason::new(Some(1), "EPERM")),
            loopback: Some(netns::Reason::new(None, "not an ack")),
            ..netns::LastFailed::default()
        },
    }
}

fn refusals() -> Vec<Refusal> {
    let mut out = vec![
        Refusal::Invalid {
            what: "cpu_weight",
            why: "0 is outside 1..=10000".to_owned(),
        },
        Refusal::NoSuchBox(id("gone")),
        Refusal::Exists(id("here")),
        Refusal::NotRunning(id("b-1"), "start"),
        Refusal::Platform {
            what: "the node agent's socket",
            platform: "macos",
            why: "no SOCK_SEQPACKET".to_owned(),
        },
        Refusal::Full(4096),
        Refusal::Exhausted {
            what: "requests in flight on one connection",
            bound: 64,
        },
        Refusal::Os {
            op: "starting a program in a box",
            why: "ENOENT".to_owned(),
        },
        Refusal::Broken("a lock was poisoned".to_owned()),
        Refusal::Lifecycle {
            id: id("retained-box"),
            cause: LifecycleCause::Pending,
            why: None,
        },
        Refusal::Lifecycle {
            id: id("retained-box"),
            cause: LifecycleCause::Uncertain,
            why: Some("its record names another native scope".to_owned()),
        },
        Refusal::Lifecycle {
            id: id("retained-box"),
            cause: LifecycleCause::Busy,
            why: None,
        },
        Refusal::Uncertain {
            id: id("unclean-box"),
            refusal: Box::new(Refusal::Broken(
                "Firecracker ended before guest readiness; its last output:\nno root".to_owned(),
            )),
            cleanup: Box::new(Refusal::Os {
                op: "attaching the box's native scope",
                why: "ESRCH".to_owned(),
            }),
        },
        // Joint plan step 12: a run that failed, in its runtime's words.
        Refusal::Failed {
            id: id("never-started"),
            why:
                "its command never started (status 128): exec: \"/bin/sh\": operation not permitted"
                    .to_owned(),
        },
        // Joint plan step 33: what a node isn't configured to serve.
        Refusal::Unconfigured {
            what: "lessors for its image VMs' credentials",
            why: "this node was given none to lease from".to_owned(),
        },
    ];
    out.extend(connect_refusals());
    out
}

/// AC-T7.4.1: an `Uncertain` refusal's causes are each any refusal but another `Uncertain`, so no message nests them
/// past one. Do: write one whose cause is itself uncertain; then read an `Uncertain` refusal's bytes with its first
/// cause's tag made `Uncertain`'s. Expect both refused by type, and the reader to stop at that tag.
#[test]
fn an_uncertain_refusals_causes_never_nest() {
    let leaf = |why: &str| Box::new(Refusal::Broken(why.to_owned()));
    let flat = Refusal::Uncertain {
        id: id("unclean-box"),
        refusal: leaf("first"),
        cleanup: leaf("second"),
    };
    let nested = Refusal::Uncertain {
        id: id("unclean-box"),
        refusal: Box::new(flat.clone()),
        cleanup: leaf("third"),
    };
    assert!(matches!(
        answer(1, &Answer::Refused(nested), MAX_MESSAGE),
        Err(Fault::Value {
            what: "uncertain refusal",
            ..
        })
    ));
    let mut bytes = answer(1, &Answer::Refused(flat), MAX_MESSAGE).unwrap();
    // The header, the refusal's tag, then the id's length and bytes: the first cause's tag follows.
    let at = HEADER_BYTES + 1 + 4 + "unclean-box".len();
    assert_eq!(bytes[at], 8, "the first cause is Broken's tag");
    bytes[at] = 12;
    let received = Received {
        bytes: &bytes,
        size: bytes.len(),
        descriptors: 0,
        descriptors_cut: false,
    };
    assert!(matches!(
        read_answer(&received, MAX_MESSAGE),
        Err(Fault::Value {
            what: "uncertain refusal",
            ..
        })
    ));
}

/// Every CONNECT cause in every phase, every named I/O kind and an unnamed one, and codes at their
/// edges, on ports at theirs.
fn connect_refusals() -> Vec<Refusal> {
    let codes = [None, Some(0), Some(-1), Some(i32::MIN), Some(i32::MAX)];
    let mut failures = vec![
        ConnectFailure::Closed,
        ConnectFailure::Reply(ConnectReply::TooLong),
        ConnectFailure::Reply(ConnectReply::NotOk),
    ];
    for phase in [
        ConnectPhase::Dial,
        ConnectPhase::Write,
        ConnectPhase::Read,
        ConnectPhase::ClearTimeouts,
    ] {
        failures.push(ConnectFailure::Expired { phase });
        for code in codes {
            failures.push(ConnectFailure::Io {
                phase,
                kind: IoKind::UNNAMED,
                code,
            });
        }
    }
    for (n, kind) in IoKind::NAMED.into_iter().enumerate() {
        failures.push(ConnectFailure::Io {
            phase: ConnectPhase::Read,
            kind: IoKind::new(kind),
            code: codes[n % codes.len()],
        });
    }
    failures
        .into_iter()
        .enumerate()
        .map(|(n, failure)| Refusal::Connect {
            port: [0, 1024, u32::MAX][n % 3],
            failure,
        })
        .collect()
}

fn answers() -> Vec<(u64, Answer)> {
    let mut out = vec![
        (
            1,
            Answer::Hello(Hello {
                build: build(),
                classes: vec![Class::Process, Class::Oci],
                limits: Limits {
                    connections: 64,
                    message_bytes: MAX_MESSAGE,
                    in_flight: 1,
                    programs: 1,
                },
                budgets: Budgets {
                    in_flight: 64,
                    programs: 1024,
                },
            }),
        ),
        (2, Answer::Info(info())),
        (3, Answer::Done),
        (6, Answer::Started { pid: 4242 }),
        (
            9,
            Answer::Usage(Usage {
                memory_bytes: 1,
                memory_peak_bytes: 2,
                oom_kills: 3,
                pids: 4,
                pids_refused: 5,
                cpu_usec: 6,
                io_read_bytes: 7,
                io_write_bytes: u64::MAX,
                guest: Some(GuestTime::PerVcpuNs(vec![0, 1, u64::MAX])),
                started_from: Some(StartedFrom::Warm),
            }),
        ),
        (
            10,
            Answer::Boxes {
                infos: vec![
                    info(),
                    BoxInfo {
                        published: Vec::new(),
                        grants: Vec::new(),
                        allocation: None,
                        route: None,
                        membership: None,
                        id: id("p"),
                        class: Class::Process,
                        state: BoxState::Empty,
                        bounds: Bounds::default(),
                        adopted: false,
                        exit: None,
                        abandoned: 0,
                        last_resort: None,
                        ended: None,
                        control: None,
                        control_wake: None,
                        credentials: Vec::new(),
                    },
                ],
                more: true,
            },
        ),
        (
            10,
            Answer::Boxes {
                infos: Vec::new(),
                more: false,
            },
        ),
        (12, Answer::Netns(counts())),
        (
            14,
            Answer::Given {
                pid: 4243,
                given: Streams::default(),
            },
        ),
    ];
    for setup in [None, Some(std::time::Duration::from_nanos(u64::MAX))] {
        out.push((
            16,
            Answer::Offered(StreamOffer {
                pid: 42,
                started: u64::MAX,
                vsock: PathBuf::from("guest/vsock"),
                token: 7,
                exec: 19,
                streams: Streams {
                    stdin: true,
                    stdout: true,
                    stderr: false,
                },
                setup,
            }),
        ));
    }
    out.extend(refusals().into_iter().map(|r| (11, Answer::Refused(r))));
    for exit in [
        Exit::Code(0),
        Exit::Code(-1),
        Exit::Signal(9),
        Exit::Unknown,
    ] {
        out.push((6, Answer::Exited(exit)));
    }
    out.push((87, Answer::PhysicalReport(example_physical_report())));
    out.push((
        201,
        Answer::BackingOffer(route_wire_backing(BackingPhase::Live)),
    ));
    // TLS_AES_256_GCM_SHA384 (0x1302, RFC 8446 §B.4) under SecP384r1MLKEM1024 (0x11ed, IANA's TLS Supported
    // Groups), and a link whose handshake named no group, in a part with more to come.
    out.push((
        204,
        Answer::Links {
            links: vec![
                Link {
                    peer: "kiln.links.test".to_owned(),
                    suite: 0x1302,
                    group: Some(0x11ed),
                },
                Link {
                    peer: "other.links.test".to_owned(),
                    suite: 0x1302,
                    group: None,
                },
            ],
            more: true,
        },
    ));
    out.push((
        204,
        Answer::Links {
            links: Vec::new(),
            more: false,
        },
    ));
    // docs/design/credential-grants.md C7: the node's lease asks of a connection's orchestrator, and its word that a
    // lease or a run's lessor is let go.
    out.push((206, Answer::LessorAsk(id("leasing-1"))));
    out.push((
        207,
        Answer::LeaseAsk {
            run: 206,
            reference: "providers/anthropic".to_owned(),
            key: Some([7; 32]),
        },
    ));
    out.push((
        208,
        Answer::LeaseAsk {
            run: 206,
            reference: "providers/openai".to_owned(),
            key: None,
        },
    ));
    out.push((206, Answer::LeaseGiven("lease-1".to_owned())));
    out.push((206, Answer::LessorEnded));
    // docs/design/credential-grants.md C8: the node's sign-in asks.
    out.push((
        209,
        Answer::CaptureAsk {
            run: 206,
            sign_in: "claude/ada".to_owned(),
            tokens: SignInTokens {
                access: Value::new(b"sk-ant-oat01-access".to_vec()),
                refresh: Some(Value::new(b"sk-ant-ort01-refresh".to_vec())),
                expires_in: Some(28_800),
                id_token: None,
                scope: Some("user:inference".to_owned()),
            },
            key: Some([3; 32]),
        },
    ));
    out.push((
        210,
        Answer::CaptureAsk {
            run: 206,
            sign_in: "openai/ada".to_owned(),
            tokens: SignInTokens {
                access: Value::new(b"at".to_vec()),
                refresh: None,
                expires_in: None,
                id_token: Some(Value::new(b"eyJhbGciOiJub25lIn0.e30.".to_vec())),
                scope: None,
            },
            key: None,
        },
    ));
    out.push((
        211,
        Answer::RefreshAsk {
            run: 206,
            sign_in: "claude/ada".to_owned(),
            key: None,
        },
    ));
    out.push((
        206,
        Answer::SignInRevoked {
            run: 206,
            sign_in: "claude/ada".to_owned(),
            key: Some([3; 32]),
        },
    ));
    out
}

fn received(bytes: &[u8], descriptors: usize) -> Received<'_> {
    Received {
        bytes,
        size: bytes.len(),
        descriptors,
        descriptors_cut: false,
    }
}

/// HEX-SOCK-2: each request reads back as the one written, with its header.
#[test]
fn every_request_reads_back_as_written() {
    for (n, req, fds) in requests() {
        let bytes = request(n, &req, MAX_MESSAGE).unwrap();
        let (h, back) = read_request(&received(&bytes, fds), MAX_MESSAGE).unwrap();
        assert_eq!((h.id, &back), (n, &req));
    }
}

/// HEX-SOCK-2: each answer reads back as the one written.
#[test]
fn every_answer_reads_back_as_written() {
    for (n, ans) in answers() {
        let bytes = answer(n, &ans, MAX_MESSAGE).unwrap();
        let (h, back) = read_answer(&received(&bytes, 0), MAX_MESSAGE).unwrap();
        assert_eq!((h.id, &back), (n, &ans));
    }
}

fn example_runtime_cost() -> RuntimeCost {
    let mib = 1 << 20;
    RuntimeCost {
        id: [7; 32],
        floor: CapacityResources {
            cpu_milli: 1000,
            mem_bytes: 64 * mib,
            disk_bytes: 0,
        },
        ceiling: CapacityResources {
            cpu_milli: 4000,
            mem_bytes: 1024 * mib,
            disk_bytes: 1 << 30,
        },
        quantum: CapacityResources {
            cpu_milli: 1000,
            mem_bytes: mib,
            disk_bytes: 1,
        },
        guest_overhead: CapacityResources {
            cpu_milli: 0,
            mem_bytes: mib,
            disk_bytes: 0,
        },
        overhead: CapacityResources {
            cpu_milli: 100,
            mem_bytes: mib,
            disk_bytes: 0,
        },
    }
}

fn example_physical_report() -> PhysicalReport {
    PhysicalReport {
        authority: "native-store-owner".into(),
        unavailable: Vec::new(),
        devices: vec![
            PhysicalDevice {
                info: DeviceInfo {
                    id: DeviceId {
                        kind: DeviceKind::Gpu,
                        key: u64::MAX,
                    },
                    model: "private-native-model".into(),
                    size: 4096,
                    holder: None,
                },
                busy_per_mille: None,
                memory_used_bytes: Some(8192),
            },
            PhysicalDevice {
                info: DeviceInfo {
                    id: DeviceId {
                        kind: DeviceKind::Gpu,
                        key: 256,
                    },
                    model: "another-native-model".into(),
                    size: 8192,
                    holder: None,
                },
                busy_per_mille: Some(0),
                memory_used_bytes: None,
            },
        ],
    }
}

/// AC-T7.4.1: the split scheduler reads the same native authority without inventing idle counters,
/// losing observed pressure or treating a factual reply as a descriptor/holder grant.
#[test]
fn physical_report_preserves_authority_unknowns_and_pressure_without_grants() {
    let req = Request::Call(Call::PhysicalReport);
    let bytes = request(87, &req, MAX_MESSAGE).unwrap();
    let (header, decoded) = read_request(&received(&bytes, 0), MAX_MESSAGE).unwrap();
    assert_eq!(header.kind, 0x0013);
    assert_eq!(decoded, req);
    let report = example_physical_report();
    let expected = Answer::PhysicalReport(report.try_clone().unwrap());
    let bytes = answer(87, &expected, MAX_MESSAGE).unwrap();
    let (header, decoded) = read_answer(&received(&bytes, 0), MAX_MESSAGE).unwrap();
    assert_eq!(header.kind, 0x800e);
    assert_eq!(decoded, expected);
    assert!(matches!(
        read_answer(&received(&bytes, 1), MAX_MESSAGE),
        Err(Fault::Descriptors { .. })
    ));
    assert!(matches!(
        answer(87, &expected, HEADER_BYTES),
        Err(Fault::Oversized { .. })
    ));
    for end in 0..bytes.len() {
        assert!(read_answer(&received(&bytes[..end], 0), MAX_MESSAGE).is_err());
    }
    let mut invalid = report.try_clone().unwrap();
    invalid.devices[0].info.holder = Some(DeviceHolder::Box(id("not-a-telemetry-grant")));
    assert!(answer(87, &Answer::PhysicalReport(invalid), MAX_MESSAGE).is_err());
    let mut invalid = report.try_clone().unwrap();
    invalid.devices.push(report.devices[0].try_clone().unwrap());
    assert!(answer(87, &Answer::PhysicalReport(invalid), MAX_MESSAGE).is_err());
}

/// AC-T7.4.1: hostile counts and impossible loads refuse before another list element is allocated,
/// without echoing native model metadata in the diagnostic.
#[test]
fn physical_report_refuses_truncated_counts_and_invalid_counter_units() {
    for count in [1, u32::try_from(MAX_DEVICES + 1).unwrap()] {
        let mut w = Writer::new(MAX_MESSAGE);
        w.header(kind::PHYSICAL_REPORT_ANSWER, 88);
        w.string("native-owner").unwrap();
        w.u32(count);
        assert!(read_answer(&received(&w.done().unwrap(), 0), MAX_MESSAGE).is_err());
    }
    let mut w = Writer::new(MAX_MESSAGE);
    w.header(kind::PHYSICAL_REPORT_ANSWER, 88);
    w.string("native-owner").unwrap();
    w.u32(1);
    put_device_id(&mut w, &example_physical_report().devices[0].info.id);
    w.string("private-native-model").unwrap();
    w.u64(4096);
    w.u8(1);
    w.u16(1001);
    w.u8(0);
    let fault = read_answer(&received(&w.done().unwrap(), 0), MAX_MESSAGE).unwrap_err();
    assert!(matches!(
        fault,
        Fault::Value {
            what: "physical report",
            ..
        }
    ));
    assert!(!fault.to_string().contains("private-native-model"));
}

/// AC-T7.4.1: peer capacity and admitted work retain the same exact cost profile/host charge;
/// neither a truncated receipt nor an invalid profile may silently become native identity cost.
#[test]
fn runtime_cost_and_admitted_allocation_survive_the_wire() {
    let cost = example_runtime_cost();
    let allocation = cost
        .allocate(CapacityResources {
            cpu_milli: 250,
            mem_bytes: 16 << 20,
            disk_bytes: 3,
        })
        .unwrap();
    let mut spec = device_spec(device_claims());
    spec.allocation = Some(allocation);
    let claimed = Request::Call(Call::Create(Box::new(spec.clone())));
    let bytes = request(55, &claimed, MAX_MESSAGE).unwrap();
    assert_eq!(
        read_request(&received(&bytes, 0), MAX_MESSAGE).unwrap().1,
        claimed
    );
    for end in 0..bytes.len() {
        assert!(read_request(&received(&bytes[..end], 0), MAX_MESSAGE).is_err());
    }
    let mut capacity = device_capacity(Vec::new());
    capacity.total = CapacityResources {
        cpu_milli: 8000,
        mem_bytes: 2 << 30,
        disk_bytes: 2 << 30,
    };
    capacity.free = capacity.total;
    capacity.cost = Some(cost);
    let offered = Answer::Capacity(Some(capacity.try_clone().unwrap()));
    let bytes = answer(55, &offered, MAX_MESSAGE).unwrap();
    assert_eq!(
        read_answer(&received(&bytes, 0), MAX_MESSAGE).unwrap().1,
        offered
    );
    for end in 0..bytes.len() {
        assert!(read_answer(&received(&bytes[..end], 0), MAX_MESSAGE).is_err());
    }
    for invalid in [
        RuntimeCost {
            id: [0; 32],
            ..cost
        },
        RuntimeCost {
            quantum: CapacityResources::default(),
            ..cost
        },
    ] {
        let mut malformed = capacity.try_clone().unwrap();
        malformed.cost = Some(invalid);
        assert!(answer(55, &Answer::Capacity(Some(malformed)), MAX_MESSAGE).is_err());
    }
    spec.allocation = Some(Allocation {
        resources: CapacityResources::default(),
        cost,
        booked: None,
    });
    assert!(matches!(
        request(
            55,
            &Request::Call(Call::Create(Box::new(spec))),
            MAX_MESSAGE
        ),
        Err(Fault::Value {
            what: "allocation",
            ..
        })
    ));
    assert_eq!(
        Refusal::from(Fault::Storage {
            what: "wire allocation receipt",
            bound: COST_BYTES
        }),
        Refusal::Exhausted {
            what: "wire allocation receipt",
            bound: COST_BYTES
        }
    );
}

/// AC-T7.4.1: inspect/list carry the original receipt, including after exit. A native
/// process observation is not permission to replace that receipt with today's cost.
#[test]
fn retained_box_allocations_survive_inspect_and_mixed_list_messages() {
    let allocation = example_runtime_cost()
        .allocate(CapacityResources {
            cpu_milli: 250,
            mem_bytes: 16 << 20,
            disk_bytes: 3,
        })
        .unwrap();
    let mut retained = info();
    retained.state = BoxState::Empty;
    retained.exit = Some(Exit::Code(0));
    retained.allocation = Some(allocation);
    for expected in [
        Answer::Info(retained.clone()),
        Answer::Boxes {
            infos: vec![info(), retained.clone(), info()],
            more: false,
        },
    ] {
        let bytes = answer(56, &expected, MAX_MESSAGE).unwrap();
        assert_eq!(
            read_answer(&received(&bytes, 0), MAX_MESSAGE).unwrap().1,
            expected
        );
        for end in 0..bytes.len() {
            assert!(read_answer(&received(&bytes[..end], 0), MAX_MESSAGE).is_err());
        }
    }
    retained.allocation = Some(Allocation {
        resources: CapacityResources::default(),
        ..allocation
    });
    assert!(matches!(
        answer(56, &Answer::Info(retained), MAX_MESSAGE),
        Err(Fault::Value {
            what: "allocation",
            ..
        })
    ));
}

/// Joint plan step 12 (ROOT, 2026-11-13: every Lifecycle Uncertain gets its reason, as a Backing one does): do answer a
/// box's Lifecycle refusal with its refusing site's words, and one with none. Expect each read back whole, the words
/// after the cause as an option, and the message ending ": <words>", or at its cause where there are none.
#[test]
fn w12_a_lifecycle_refusal_carries_its_reason() {
    let words = "its record names another native scope";
    let worded = Refusal::Lifecycle {
        id: id("retained-box"),
        cause: LifecycleCause::Uncertain,
        why: Some(words.to_owned()),
    };
    let bare = Refusal::Lifecycle {
        id: id("retained-box"),
        cause: LifecycleCause::Uncertain,
        why: None,
    };
    for refused in [&worded, &bare] {
        let answered = Answer::Refused(refused.clone());
        let bytes = answer(42, &answered, MAX_MESSAGE).unwrap();
        assert_eq!(
            read_answer(&received(&bytes, 0), MAX_MESSAGE).unwrap().1,
            answered
        );
    }
    assert!(
        worded.to_string().ends_with(&format!(": {words}")),
        "{worded}"
    );
    assert!(
        bare.to_string()
            .ends_with("physical termination or absence has not been proved"),
        "{bare}"
    );
}

/// AC-T7.4.1: retained-resource lifecycle causes survive the wire as distinct typed outcomes;
/// an unknown cause cannot become absence, success or a default retry decision.
#[test]
fn lifecycle_causes_preserve_box_identity_and_reject_unknown_tags() {
    for (cause, why) in [
        (LifecycleCause::Pending, None),
        (
            LifecycleCause::Uncertain,
            Some("its record names another native scope".to_owned()),
        ),
        (LifecycleCause::Busy, None),
        (LifecycleCause::NodeVmEnded, None),
    ] {
        let worded = why.is_some();
        let refused = Answer::Refused(Refusal::Lifecycle {
            id: id("retained-box"),
            cause,
            why,
        });
        let bytes = answer(41, &refused, MAX_MESSAGE).unwrap();
        assert_eq!(
            read_answer(&received(&bytes, 0), MAX_MESSAGE).unwrap().1,
            refused
        );
        if worded {
            continue;
        }
        // 3 is Retired (joint plan step 14), 4 NodeVmEnded (joint plan step 39, A-6); the next tag is unknown. The
        // cause is the byte before its reason's option, a 0 where it gives none (joint plan step 12).
        for tag in [5, u8::MAX] {
            let mut invalid = bytes.clone();
            let at = invalid.len() - 2;
            invalid[at] = tag;
            let error = read_answer(&received(&invalid, 0), MAX_MESSAGE).unwrap_err();
            assert!(matches!(
                error,
                Fault::Value {
                    what: "lifecycle cause",
                    ..
                }
            ));
        }
    }
}

/// HEX-SOCK-2: a given answer carries a descriptor for each stream it names, and is refused, naming the count,
/// with any other.
#[test]
fn a_given_answer_carries_a_descriptor_for_each_stream_it_names() {
    let given = Answer::Given {
        pid: 4243,
        given: Streams {
            stdin: false,
            stdout: true,
            stderr: true,
        },
    };
    let bytes = answer(14, &given, MAX_MESSAGE).unwrap();
    assert_eq!(
        read_answer(&received(&bytes, 2), MAX_MESSAGE).map(|(_, a)| a),
        Ok(given)
    );
    for fds in [0, 1, 3] {
        assert_eq!(
            read_answer(&received(&bytes, fds), MAX_MESSAGE),
            Err(Fault::Descriptors {
                what: "a given answer",
                want: 2,
                got: fds
            })
        );
    }
}

/// Native plan step 8, guest stdio (HEX-GUEST-41): a client's word that it took a given answer's ends, by its exec's
/// id, is a header alone, kind 0x000e beside the given answer's; read back as itself; and it carries no descriptor,
/// one that comes with it refused, naming the count.
#[test]
fn a_word_that_a_given_answers_ends_were_taken_is_a_header_alone() {
    let taken = request(14, &Request::Taken, MAX_MESSAGE).unwrap();
    assert_eq!(taken, header(0x000e, 14));
    assert_eq!(
        read_request(&received(&taken, 0), MAX_MESSAGE),
        Ok((
            Header {
                kind: 0x000e,
                id: 14
            },
            Request::Taken
        ))
    );
    assert_eq!(
        read_request(&received(&taken, 1), MAX_MESSAGE).map_err(|bad| bad.fault),
        Err(Fault::Descriptors {
            what: "a taken",
            want: 0,
            got: 1
        })
    );
}

/// Every message's header: its magic, this side's wire identity, its kind and its id.
fn header(kind: u16, id: u64) -> Vec<u8> {
    let mut b = MAGIC.to_vec();
    b.extend_from_slice(&IDENTITY.to_le_bytes());
    b.extend_from_slice(&kind.to_le_bytes());
    b.extend_from_slice(&id.to_le_bytes());
    b
}

/// `bytes` as a message of the wire whose identity is `other`.
fn of_wire(mut bytes: Vec<u8>, other: u64) -> Vec<u8> {
    bytes[4..12].copy_from_slice(&other.to_le_bytes());
    bytes
}

/// One wire (the lead's word of 2026-11-09): a message's header is its magic, its sender's wire identity, its kind
/// and its id, in that order and at those bytes, and a hello's body begins with its sender's build. Do: write a hello
/// and its answer. Expect: each starts with the header, then the build as a string, the layout every build keeps.
#[test]
fn a_hellos_head_is_the_header_then_its_senders_build() {
    let b = build();
    let mut built = u32::try_from(b.len()).unwrap().to_le_bytes().to_vec();
    built.extend_from_slice(b.as_bytes());
    let hello = request(5, &Request::Hello { build: b.clone() }, MAX_MESSAGE).unwrap();
    assert_eq!(hello.len(), HEADER_BYTES + built.len());
    assert_eq!(&hello[..HEADER_BYTES], &header(0x0001, 5)[..]);
    assert_eq!(&hello[HEADER_BYTES..], &built[..]);
    let answered = answer(5, &answers()[0].1, MAX_MESSAGE).unwrap();
    assert_eq!(&answered[..HEADER_BYTES], &header(0x8001, 5)[..]);
    assert_eq!(
        &answered[HEADER_BYTES..HEADER_BYTES + built.len()],
        &built[..]
    );
}

/// One wire (the lead's word of 2026-11-09): a message of another wire is refused before its body is read, naming
/// both wires, and a hello's or a hello's answer's sender's build with them. Do: read a hello, a hello's answer, an
/// inspect and a done, each stamped with another identity. Expect: each refused as of another wire, naming that
/// identity; the hello's and the answer's naming their sender's build too; and as a refusal, `Invalid` of "wire"
/// naming both identities and both builds.
#[test]
fn a_message_of_another_wire_is_refused_naming_both_wires() {
    let other = IDENTITY ^ 1;
    let theirs = "hex 0.0.0 (another commit)".to_owned();
    let hello = of_wire(
        request(
            5,
            &Request::Hello {
                build: theirs.clone(),
            },
            MAX_MESSAGE,
        )
        .unwrap(),
        other,
    );
    let got = read_request(&received(&hello, 0), MAX_MESSAGE).unwrap_err();
    assert_eq!(
        (got.id, &got.fault),
        (
            5,
            &Fault::Wire {
                theirs: other,
                build: Some(theirs.clone())
            }
        )
    );
    let answered = of_wire(
        answer(
            5,
            &Answer::Hello(Hello {
                build: theirs.clone(),
                ..match &answers()[0].1 {
                    Answer::Hello(h) => h.clone(),
                    other => panic!("{other:?}"),
                }
            }),
            MAX_MESSAGE,
        )
        .unwrap(),
        other,
    );
    let fault = read_answer(&received(&answered, 0), MAX_MESSAGE).unwrap_err();
    assert_eq!(
        fault,
        Fault::Wire {
            theirs: other,
            build: Some(theirs.clone())
        }
    );
    let refusal = Refusal::from(fault);
    let Refusal::Invalid { what: "wire", why } = &refusal else {
        panic!("{refusal:?}")
    };
    for named in [
        format!("{other:016x}"),
        format!("{IDENTITY:016x}"),
        theirs.clone(),
        build(),
    ] {
        assert!(why.contains(&named), "{why} names no {named}");
    }
    let inspect = of_wire(request(8, &requests()[7].1, MAX_MESSAGE).unwrap(), other);
    assert_eq!(
        read_request(&received(&inspect, 0), MAX_MESSAGE)
            .unwrap_err()
            .fault,
        Fault::Wire {
            theirs: other,
            build: None
        }
    );
    let done = of_wire(answer(3, &Answer::Done, MAX_MESSAGE).unwrap(), other);
    assert_eq!(
        read_answer(&received(&done, 0), MAX_MESSAGE),
        Err(Fault::Wire {
            theirs: other,
            build: None
        })
    );
}

/// One wire (the lead's word of 2026-11-09): the identity is FNV-1a of the wire definitions' source, this module's
/// and the stream module's, with no number to bump, and a checkout's line endings don't split it. Do: digest the
/// sources as read now, then with every line ended `\r\n` (a CRLF checkout's, Windows' by default), then with every
/// line ended `\n` alone (an LF checkout's). Expect: each [`IDENTITY`]; and a source with one byte more another
/// identity.
#[test]
fn the_wire_identity_is_its_definitions_digest_line_endings_aside() {
    let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
    let paths = [
        "wire.rs",
        "stream.rs",
        "schedule.rs",
        "netns.rs",
        "../../hex-sim/src/peer.rs",
        "../../hex-sim/src/model.rs",
        "../../hex-api/src/spec.rs",
        "../../hex-api/src/id.rs",
        "../../hex-api/src/bounds.rs",
        "../../hex-api/src/devices.rs",
        "../../hex-api/src/runtime.rs",
        "../../hex-api/src/grants.rs",
        "../../hex-api/src/credentials.rs",
        "../../hex-api/src/lease.rs",
    ];
    let mut sources: Vec<Vec<u8>> = paths
        .iter()
        .map(|p| std::fs::read(dir.join(p)).unwrap())
        .collect();
    let digest =
        |sources: &[Vec<u8>]| identity(&sources.iter().map(Vec::as_slice).collect::<Vec<_>>());
    assert_eq!(digest(&sources), IDENTITY);
    let crlf = |bytes: &[u8]| -> Vec<u8> {
        let mut out = Vec::new();
        for &b in bytes.iter().filter(|&&b| b != b'\r') {
            if b == b'\n' {
                out.push(b'\r');
            }
            out.push(b);
        }
        out
    };
    assert_eq!(
        digest(&sources.iter().map(|s| crlf(s)).collect::<Vec<_>>()),
        IDENTITY
    );
    let lf = |bytes: &[u8]| -> Vec<u8> { bytes.iter().copied().filter(|&b| b != b'\r').collect() };
    assert_eq!(
        digest(&sources.iter().map(|s| lf(s)).collect::<Vec<_>>()),
        IDENTITY
    );
    for changed in 0..sources.len() {
        sources[changed].push(b' ');
        assert_ne!(digest(&sources), IDENTITY);
        sources[changed].pop();
    }
}

/// HEX-SOCK-2: a list longer than one message carries is split into messages within the bound, each
/// but the last marked `more`, and the parts read back as the whole list, in order.
#[test]
fn a_long_list_is_split_into_messages_within_the_bound() {
    let infos: Vec<BoxInfo> = (0..5_000)
        .map(|n| BoxInfo {
            allocation: None,
            route: None,
            membership: None,
            id: id(&format!("box-{n:06}")),
            ..info()
        })
        .collect();
    let parts = boxes(10, &infos, MAX_MESSAGE).unwrap();
    assert!(parts.len() > 1, "5,000 boxes fit one message");
    let mut back = Vec::new();
    for (n, bytes) in parts.iter().enumerate() {
        assert!(
            bytes.len() <= MAX_MESSAGE,
            "part {n} is {} bytes",
            bytes.len()
        );
        match read_answer(&received(bytes, 0), MAX_MESSAGE).unwrap() {
            (Header { id: 10, .. }, Answer::Boxes { infos, more }) => {
                assert_eq!(more, n + 1 < parts.len(), "part {n}'s more");
                back.extend(infos);
            }
            other => panic!("{other:?}"),
        }
    }
    assert_eq!(back, infos);
    let one = boxes(10, &[], MAX_MESSAGE).unwrap();
    assert_eq!(one.len(), 1);
    assert_eq!(
        read_answer(&received(&one[0], 0), MAX_MESSAGE).unwrap().1,
        Answer::Boxes {
            infos: Vec::new(),
            more: false
        }
    );
}

/// HEX-QUIC-30 (docs/design/hex-quic.md Q12): a node's links report longer than one message carries is split as a
/// list is, each message within the bound and each but the last marked `more`, and the parts read back as the whole
/// report, in order. Do: split 2,000 links with 253-byte names, and none. Expect: more than one part, each within the
/// bound, read back whole; none as one part.
#[test]
fn hex_quic_30_a_long_links_report_is_split_into_messages_within_the_bound() {
    let all: Vec<Link> = (0..2_000)
        .map(|n| Link {
            peer: format!("{n:06}-{}", "l".repeat(246)),
            suite: 0x1302,
            group: Some(0x11ed),
        })
        .collect();
    let parts = links(10, &all, MAX_MESSAGE).unwrap();
    assert!(parts.len() > 1, "2,000 links fit one message");
    let mut back = Vec::new();
    for (n, bytes) in parts.iter().enumerate() {
        assert!(
            bytes.len() <= MAX_MESSAGE,
            "part {n} is {} bytes",
            bytes.len()
        );
        match read_answer(&received(bytes, 0), MAX_MESSAGE).unwrap() {
            (Header { id: 10, .. }, Answer::Links { links, more }) => {
                assert_eq!(more, n + 1 < parts.len(), "part {n}'s more");
                back.extend(links);
            }
            other => panic!("{other:?}"),
        }
    }
    assert_eq!(back, all);
    let one = links(10, &[], MAX_MESSAGE).unwrap();
    assert_eq!(one.len(), 1);
    assert_eq!(
        read_answer(&received(&one[0], 0), MAX_MESSAGE).unwrap().1,
        Answer::Links {
            links: Vec::new(),
            more: false
        }
    );
}

/// HEX-SOCK-1: every truncation of every request and answer is refused, by type, never a panic.
#[test]
fn every_truncation_is_refused() {
    for (n, req, fds) in requests() {
        let bytes = request(n, &req, MAX_MESSAGE).unwrap();
        for cut in 0..bytes.len() {
            let got = read_request(&received(&bytes[..cut], fds), MAX_MESSAGE);
            assert!(got.is_err(), "{req:?} cut at {cut} read as {got:?}");
        }
    }
    for (n, ans) in answers() {
        let bytes = answer(n, &ans, MAX_MESSAGE).unwrap();
        for cut in 0..bytes.len() {
            let got = read_answer(&received(&bytes[..cut], 0), MAX_MESSAGE);
            assert!(got.is_err(), "{ans:?} cut at {cut} read as {got:?}");
        }
    }
}

/// HEX-SOCK-1: a message whose header is cut, or isn't Hex's, is refused with id 0 (there is no
/// request to answer by its own); past the header, the refusal carries the request's id.
#[test]
fn a_refusal_goes_to_the_request_its_header_names() {
    let got = read_request(&received(b"HEXW\x01\x00", 0), MAX_MESSAGE).unwrap_err();
    assert_eq!(got.id, 0);
    assert!(matches!(got.fault, Fault::Truncated { .. }), "{got:?}");
    let got = read_request(&received(b"GET / HTTP/1.1\r\n\r\n", 0), MAX_MESSAGE).unwrap_err();
    assert_eq!((got.id, got.fault), (0, Fault::Magic));
    let mut bytes = request(77, &requests()[4].1, MAX_MESSAGE).unwrap();
    bytes.push(0);
    let got = read_request(&received(&bytes, 0), MAX_MESSAGE).unwrap_err();
    assert_eq!(got.id, 77);
    assert_eq!(
        got.fault,
        Fault::Leftover {
            bytes: 1,
            kind: 0x0003,
        }
    );
}

/// HEX-SOCK-1: a message over the bound is refused naming it, by the size the kernel reports even
/// when the receiver's buffer cut it, and with the request's id.
#[test]
fn an_oversized_message_is_refused_naming_the_bound() {
    let bytes = request(9, &requests()[8].1, MAX_MESSAGE).unwrap();
    let cut = Received {
        bytes: &bytes,
        size: MAX_MESSAGE + 1,
        descriptors: 0,
        descriptors_cut: false,
    };
    let got = read_request(&cut, MAX_MESSAGE).unwrap_err();
    assert_eq!(got.id, 9);
    assert_eq!(
        got.fault,
        Fault::Oversized {
            bytes: MAX_MESSAGE + 1,
            bound: MAX_MESSAGE
        }
    );
    assert_eq!(
        Refusal::from(got.fault),
        Refusal::Invalid {
            what: "message",
            why: format!(
                "{} bytes is over the {MAX_MESSAGE}-byte bound on one message",
                MAX_MESSAGE + 1
            ),
        }
    );
    // A writer refuses before it sends: a program past the bound is never copied whole.
    let huge = Call::Exec {
        id: id("b"),
        program: Program::of(&[&"x".repeat(MAX_MESSAGE)]),
        streams: Streams::default(),
    };
    assert!(matches!(
        request(1, &Request::Call(huge), MAX_MESSAGE),
        Err(Fault::Oversized {
            bound: MAX_MESSAGE,
            ..
        })
    ));
}

/// HEX-SOCK-2 (runtime design goal 7, decision 7): a run is one request, the wire's kind after every other
/// request's (0x000f): a create's spec, then an exec's program, the streams it carries and those it asks for, as an
/// exec asking for streams has them. Do: write one carrying stdout and stderr and asking for stdin; read it with two
/// descriptors, with none, one and three, and apart from its stdio. Expect: read back as written with two, and whole
/// apart from its stdio (a stream transport's, whose streams travel on connections of their own); refused naming
/// the count otherwise.
#[test]
fn a_run_is_a_create_and_an_exec_in_one_request() {
    let run = run();
    let bytes = request(15, &run, MAX_MESSAGE).unwrap();
    assert_eq!(&bytes[12..14], &0x000fu16.to_le_bytes());
    let (header, decoded) = read_request(&received(&bytes, 2), MAX_MESSAGE).unwrap();
    assert_eq!(
        header,
        Header {
            kind: 0x000f,
            id: 15
        }
    );
    assert_eq!(decoded, run);
    for fds in [0, 1, 3] {
        assert_eq!(
            read_request(&received(&bytes, fds), MAX_MESSAGE)
                .unwrap_err()
                .fault,
            Fault::Descriptors {
                what: "a run",
                want: 2,
                got: fds
            }
        );
    }
    assert_eq!(read_request_apart(&bytes, MAX_MESSAGE).unwrap().1, run);
}

/// HEX-SOCK-1: a kind the wire doesn't define is refused by type, a request's kinds as an answer and an
/// answer's as a request too.
#[test]
fn unknown_kinds_are_refused() {
    let hello = request(5, &Request::Hello { build: build() }, MAX_MESSAGE).unwrap();
    for kind in [0u16, 0x0022, 0x7fff, 0x8001, 0x8009, 0xffff] {
        let mut bytes = hello.clone();
        bytes[12..14].copy_from_slice(&kind.to_le_bytes());
        let got = read_request(&received(&bytes, 0), MAX_MESSAGE).unwrap_err();
        assert!(
            matches!(got.fault, Fault::Undefined { kind: k, expected: "a request" } if k == kind),
            "{got:?}"
        );
    }
    for kind in [0u16, 0x0001, 0x000b, 0x8000, 0xffff] {
        let mut bytes = answer(5, &Answer::Done, MAX_MESSAGE).unwrap();
        bytes[12..14].copy_from_slice(&kind.to_le_bytes());
        assert!(matches!(
            read_answer(&received(&bytes, 0), MAX_MESSAGE),
            Err(Fault::Undefined {
                expected: "an answer",
                ..
            })
        ));
    }
}

/// HEX-SOCK-1: descriptors travel only with an exec, one for each stream it names; any other count,
/// or more than the three a message takes, is refused by type.
#[test]
fn descriptors_where_none_belong_are_refused() {
    let create = request(2, &requests()[1].1, MAX_MESSAGE).unwrap();
    let got = read_request(&received(&create, 1), MAX_MESSAGE).unwrap_err();
    assert_eq!(
        (got.id, got.fault),
        (
            2,
            Fault::Descriptors {
                what: "a create",
                want: 0,
                got: 1
            }
        )
    );
    let exec = request(6, &requests()[5].1, MAX_MESSAGE).unwrap();
    for fds in [0, 1, 3] {
        let got = read_request(&received(&exec, fds), MAX_MESSAGE).unwrap_err();
        assert_eq!(
            got.fault,
            Fault::Descriptors {
                what: "an exec",
                want: 2,
                got: fds
            }
        );
    }
    let cut = Received {
        bytes: &exec,
        size: exec.len(),
        descriptors: 3,
        descriptors_cut: true,
    };
    assert_eq!(
        read_request(&cut, MAX_MESSAGE).unwrap_err().fault,
        Fault::DescriptorsCut
    );
    let done = answer(3, &Answer::Done, MAX_MESSAGE).unwrap();
    assert!(matches!(
        read_answer(&received(&done, 1), MAX_MESSAGE),
        Err(Fault::Descriptors {
            want: 0,
            got: 1,
            ..
        })
    ));
}

/// HEX-SOCK-1: a body whose values break the seam's own rules is refused by name: an id that is a
/// path, a signal outside 1..=64, a boolean or option byte that is neither, streams past stderr, text
/// that isn't UTF-8, and a list claiming more items than bytes are left.
#[test]
fn hostile_values_are_refused_by_name() {
    let head = |kind: u16| header(kind, 42);
    let string = |s: &[u8]| {
        let mut b = u32::try_from(s.len()).unwrap().to_le_bytes().to_vec();
        b.extend_from_slice(s);
        b
    };
    let value = |bytes: &[u8], fds| match read_request(&received(bytes, fds), MAX_MESSAGE) {
        Err(Bad {
            id: 42,
            fault: Fault::Value { what, .. },
            ..
        }) => what,
        other => panic!("{other:?}"),
    };
    let mut start = head(0x0003);
    start.extend(string(b"../etc"));
    assert_eq!(value(&start, 0), "box id");
    let mut start = head(0x0003);
    start.extend(string(b"\xff\xfe"));
    assert_eq!(value(&start, 0), "box id");
    for n in [0u8, 65, 255] {
        let mut signal = head(0x0005);
        signal.extend(string(b"b"));
        signal.push(n);
        assert_eq!(value(&signal, 0), "signal");
    }
    let mut exec = head(0x0004);
    exec.extend(string(b"b"));
    exec.extend([0, 0, 0, 0, 0, 0, 0, 0, 7]); // no args, no env, then an option byte of 7
    assert_eq!(value(&exec, 0), "option");
    let mut exec = head(0x0004);
    exec.extend(string(b"b"));
    exec.extend([0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0b1000]);
    assert_eq!(value(&exec, 0), "streams");
    let mut create = head(0x0002);
    create.extend(string(b"b"));
    create.extend([0; 8]);
    create.push(9); // no such class spec
    assert_eq!(value(&create, 0), "class spec");
    // A list claiming four billion items in a short message is refused before room is made for them.
    let mut exec = head(0x0004);
    exec.extend(string(b"b"));
    exec.extend(u32::MAX.to_le_bytes());
    assert!(matches!(
        read_request(&received(&exec, 0), MAX_MESSAGE)
            .unwrap_err()
            .fault,
        Fault::Truncated { .. }
    ));
    let mut info = answer(2, &Answer::Info(info()), MAX_MESSAGE).unwrap();
    // None route/allocation tags follow abandoned's eight bytes and the exit's five plus tag, and the
    // empty grant and published lists' counts, the last resort's and the run's words' none tags, the empty
    // credential list's count and the agent-control endpoint's none tag end the info.
    let adopted = info.len()
        - NO_GRANTS
        - NO_END
        - NO_WORDS
        - NO_CREDENTIALS
        - NO_CONTROL
        - NO_WAKE
        - 2
        - 8
        - 6
        - 1;
    info[adopted] = 2;
    assert!(matches!(
        read_answer(&received(&info, 0), MAX_MESSAGE),
        Err(Fault::Value {
            what: "adopted",
            ..
        })
    ));
}

/// The refusal names a decoded refusal carries are kept once, so the same name is the same string
/// however often it comes, and a name past the bound is refused, never kept.
#[test]
fn refusal_names_are_kept_once_and_bounded() {
    let bytes = answer(1, &Answer::Refused(refusals()[0].clone()), MAX_MESSAGE).unwrap();
    let what = |bytes: &[u8]| match read_answer(&received(bytes, 0), MAX_MESSAGE).unwrap().1 {
        Answer::Refused(Refusal::Invalid { what, .. }) => what,
        other => panic!("{other:?}"),
    };
    let (a, b) = (what(&bytes), what(&bytes));
    assert_eq!(a, "cpu_weight");
    assert!(std::ptr::eq(a, b), "a name is kept once");
    let long = Refusal::Invalid {
        what: "x",
        why: String::new(),
    };
    let mut bytes = answer(1, &Answer::Refused(long), MAX_MESSAGE).unwrap();
    // Swap the one-byte name for one longer than the bound.
    let at = HEADER_BYTES + 1;
    let name = "n".repeat(NAME_BYTES + 1);
    let mut body = u32::try_from(name.len()).unwrap().to_le_bytes().to_vec();
    body.extend_from_slice(name.as_bytes());
    bytes.splice(at..at + 5, body);
    assert!(matches!(
        read_answer(&received(&bytes, 0), MAX_MESSAGE),
        Err(Fault::Value {
            what: "refusal name",
            ..
        })
    ));
}

fn any_id() -> impl Strategy<Value = BoxId> {
    "[A-Za-z0-9_+-][A-Za-z0-9_+.-]{0,15}".prop_map(|s| BoxId::new(&s).unwrap())
}

fn any_text() -> impl Strategy<Value = String> {
    "\\PC{0,12}"
}

/// Any path: on Unix any bytes but NUL, which needn't be UTF-8; elsewhere any text, which a path
/// there is.
#[cfg(unix)]
fn any_path() -> impl Strategy<Value = PathBuf> {
    proptest::collection::vec(any::<u8>().prop_filter("no NUL", |b| *b != 0), 0..24).prop_map(|b| {
        use std::os::unix::ffi::OsStrExt;
        PathBuf::from(std::ffi::OsStr::from_bytes(&b))
    })
}

#[cfg(not(unix))]
fn any_path() -> impl Strategy<Value = PathBuf> {
    "[^\0]{0,24}".prop_map(PathBuf::from)
}

fn any_bounds() -> impl Strategy<Value = Bounds> {
    (
        (
            any::<Option<u16>>(),
            any::<Option<u32>>(),
            any::<Option<u64>>(),
            any::<Option<u64>>(),
        ),
        (
            any::<Option<u16>>(),
            any::<Option<u32>>(),
            any::<Option<(Option<u64>, Option<u64>, Option<u64>, Option<u64>)>>(),
            any::<Option<u32>>(),
        ),
    )
        .prop_map(|((cw, cm, mm, mh), (iw, il, io, pm))| Bounds {
            cpu_weight: cw,
            cpu_max_milli: cm,
            memory_max: mm,
            memory_high: mh,
            io_weight: iw,
            io_latency_us: il,
            io_max: io.map(|(a, b, c, d)| IoRates {
                read_bps: a,
                write_bps: b,
                read_iops: c,
                write_iops: d,
            }),
            pids_max: pm,
        })
}

fn any_program() -> impl Strategy<Value = Program> {
    (
        proptest::collection::vec(any_text(), 0..4),
        proptest::collection::vec((any_text(), any_text()), 0..3),
        proptest::option::of(any_text()),
        proptest::option::of((
            any::<u32>(),
            any::<u32>(),
            proptest::collection::vec(any::<u32>(), 0..3),
        )),
    )
        .prop_map(|(args, env, cwd, user)| Program {
            args,
            env,
            cwd,
            user: user.map(|(uid, gid, groups)| User { uid, gid, groups }),
        })
}

fn any_spec() -> impl Strategy<Value = BoxSpec> {
    let oci = (
        prop_oneof![
            any_path().prop_map(Rootfs::Dir),
            proptest::collection::vec(any_path(), 0..3).prop_map(Rootfs::Layers),
        ],
        any_program(),
        proptest::collection::vec((any_path(), any_text(), any::<bool>()), 0..3),
        proptest::option::of(any_text()),
        any::<bool>(),
        proptest::collection::vec((any_text(), any_text()), 0..3),
    )
        .prop_map(|(rootfs, init, mounts, hostname, host, hosts)| {
            Box::new(OciSpec {
                rootfs,
                init,
                mounts: mounts
                    .into_iter()
                    .map(|(source, destination, read_only)| Mount {
                        source,
                        destination,
                        read_only,
                    })
                    .collect(),
                hostname,
                network: if host {
                    Network::Host
                } else {
                    Network::Private
                },
                hosts,
            })
        });
    let workload = (0u8..3, oci).prop_map(|(backend, spec)| match backend {
        0 => ClassSpec::Oci(spec),
        1 => ClassSpec::Firecracker(spec),
        _ => ClassSpec::Shards(spec),
    });
    let class = prop_oneof![
        Just(ClassSpec::Process),
        any_path().prop_map(ClassSpec::Bundle),
        workload,
    ];
    let owner = proptest::option::of(("\\PC{0,16}", any::<bool>()));
    (any_id(), any_bounds(), class, owner).prop_map(|(id, bounds, class, owner)| BoxSpec {
        allocation: None,
        route: None,
        devices: Vec::new(),
        id,
        bounds,
        class,
        owner: owner.map(|(name, reuse)| BoxOwner { name, reuse }),
        band: 0,
    })
}

fn any_exit() -> impl Strategy<Value = Exit> {
    prop_oneof![
        any::<i32>().prop_map(Exit::Code),
        any::<i32>().prop_map(Exit::Signal),
        Just(Exit::Unknown),
    ]
}

fn any_info() -> impl Strategy<Value = BoxInfo> {
    (
        any_id(),
        0u8..4,
        0u8..3,
        any_bounds(),
        any::<bool>(),
        proptest::option::of(any_exit()),
        any::<u64>(),
    )
        .prop_map(
            |(id, backend, state, bounds, adopted, exit, abandoned)| BoxInfo {
                published: Vec::new(),
                grants: Vec::new(),
                allocation: None,
                route: None,
                membership: None,
                id,
                class: match backend {
                    0 => Class::Process,
                    1 => Class::Oci,
                    2 => Class::Firecracker,
                    _ => Class::Shards,
                },
                state: match state {
                    0 => BoxState::Empty,
                    1 => BoxState::Running,
                    _ => BoxState::Created,
                },
                bounds,
                adopted,
                exit,
                abandoned,
                last_resort: None,
                ended: None,
                control: None,
                control_wake: None,
                credentials: Vec::new(),
            },
        )
}

/// The names a generated refusal carries, from a fixed few: the names this process keeps are
/// bounded, and random ones would spend that bound across cases.
const SOME_NAMES: [&str; 4] = ["cpu_weight", "start", "connections", "an op"];

fn any_refusal() -> impl Strategy<Value = Refusal> {
    prop_oneof![
        any_cause(),
        (any_id(), any_cause(), any_cause()).prop_map(|(id, refusal, cleanup)| {
            Refusal::Uncertain {
                id,
                refusal: Box::new(refusal),
                cleanup: Box::new(cleanup),
            }
        }),
    ]
}

/// Any refusal but an `Uncertain` one: what an `Uncertain` refusal's causes are.
fn any_cause() -> impl Strategy<Value = Refusal> {
    let name = proptest::sample::select(&SOME_NAMES[..]);
    prop_oneof![
        (name.clone(), any_text()).prop_map(|(what, why)| Refusal::Invalid { what, why }),
        any_id().prop_map(Refusal::NoSuchBox),
        any_id().prop_map(Refusal::Exists),
        (any_id(), name.clone()).prop_map(|(id, op)| Refusal::NotRunning(id, op)),
        (name.clone(), name.clone(), any_text()).prop_map(|(what, platform, why)| {
            Refusal::Platform {
                what,
                platform,
                why,
            }
        }),
        any::<u32>().prop_map(|n| Refusal::Full(usize::try_from(n).unwrap())),
        (name.clone(), any::<u32>()).prop_map(|(what, bound)| Refusal::Exhausted {
            what,
            bound: usize::try_from(bound).unwrap()
        }),
        (name, any_text()).prop_map(|(op, why)| Refusal::Os { op, why }),
        any_text().prop_map(Refusal::Broken),
        (
            any_id(),
            proptest::sample::select(vec![
                LifecycleCause::Pending,
                LifecycleCause::Uncertain,
                LifecycleCause::Busy
            ]),
            proptest::option::of(any_text())
        )
            .prop_map(|(id, cause, why)| Refusal::Lifecycle { id, cause, why }),
        (any::<u32>(), any_connect_failure())
            .prop_map(|(port, failure)| Refusal::Connect { port, failure }),
        proptest::sample::select(Upstream::ALL.to_vec())
            .prop_map(|needs| Refusal::Awaits { needs }),
        (any_id(), any_text()).prop_map(|(id, why)| Refusal::Failed { id, why }),
        (proptest::sample::select(&SOME_NAMES[..]), any_text())
            .prop_map(|(what, why)| Refusal::Unconfigured { what, why }),
        (
            proptest::sample::select(&SOME_NAMES[..]),
            any_guest_failure()
        )
            .prop_map(|(op, failure)| Refusal::Guest { op, failure }),
    ]
}

/// Each of a Shards guest's failures (the review's B-15), its durations whole nanoseconds of a u64, as Shards keeps
/// them.
fn any_guest_failure() -> impl Strategy<Value = GuestFailure> {
    let nanos = || any::<u64>().prop_map(std::time::Duration::from_nanos);
    prop_oneof![
        (nanos(), nanos(), nanos()).prop_map(|(waited, over, slowest)| GuestFailure::Stalled {
            waited,
            over,
            slowest
        }),
        prop_oneof![
            Just(GuestEnd::PowerOff),
            Just(GuestEnd::Reset),
            Just(GuestEnd::Stopped),
            any_text().prop_map(GuestEnd::Failed),
        ]
        .prop_map(GuestFailure::Ended),
        (any::<u32>(), any_text()).prop_map(|(pid, why)| GuestFailure::Gone { pid, why }),
        any_text().prop_map(GuestFailure::Rate),
        any_text().prop_map(GuestFailure::HostUse),
    ]
}

fn any_connect_failure() -> impl Strategy<Value = ConnectFailure> {
    let phase = proptest::sample::select(vec![
        ConnectPhase::Dial,
        ConnectPhase::Write,
        ConnectPhase::Read,
        ConnectPhase::ClearTimeouts,
    ]);
    let kind = prop_oneof![
        proptest::sample::select(IoKind::NAMED.to_vec()).prop_map(IoKind::new),
        Just(IoKind::UNNAMED),
    ];
    prop_oneof![
        phase
            .clone()
            .prop_map(|phase| ConnectFailure::Expired { phase }),
        (phase, kind, proptest::option::of(any::<i32>()))
            .prop_map(|(phase, kind, code)| ConnectFailure::Io { phase, kind, code }),
        Just(ConnectFailure::Closed),
        Just(ConnectFailure::Reply(ConnectReply::TooLong)),
        Just(ConnectFailure::Reply(ConnectReply::NotOk)),
    ]
}

proptest! {
    /// HEX-SOCK-2: any create, any exec and any run (runtime design goal 7) read back as written.
    #[test]
    fn any_create_or_exec_reads_back(spec in any_spec(), program in any_program(), id in any_id(), n in any::<u64>(), bits in 0u8..8) {
        let req = Request::Call(Call::Create(Box::new(spec.clone())));
        let bytes = request(n, &req, MAX_MESSAGE).unwrap();
        prop_assert_eq!(read_request(&received(&bytes, 0), MAX_MESSAGE).unwrap(), (Header { kind: 0x0002, id: n }, req));
        let streams = Streams::of(bits).unwrap();
        let req = Request::Call(Call::Exec { id, program: program.clone(), streams });
        let bytes = request(n, &req, MAX_MESSAGE).unwrap();
        prop_assert_eq!(read_request(&received(&bytes, streams.count()), MAX_MESSAGE).unwrap().1, req);
        let ask = Streams::of(!bits & 0b111).unwrap();
        let req = Request::Call(Call::Run { spec: Box::new(spec), program, streams, ask });
        let bytes = request(n, &req, MAX_MESSAGE).unwrap();
        prop_assert_eq!(read_request(&received(&bytes, streams.count()), MAX_MESSAGE).unwrap().1, req);
    }

    /// HEX-SOCK-2: any box info, list, refusal and network namespace counts read back as written.
    #[test]
    fn any_answer_reads_back(infos in proptest::collection::vec(any_info(), 0..4), refusal in any_refusal(), exit in any_exit(), n in any::<[u64; 11]>(), bits in 0u8..4, errno in proptest::option::of(any::<i32>()), text in "[a-zé ]{0,300}") {
        let counts = netns::Counts {
            made_ahead: n[0], taken: n[1], made_on_path: n[2], discarded: n[3], failed: n[4],
            failed_by: netns::FailedBy { home: n[5], unshare: n[6], open: n[7], loopback: n[8], back: n[9] },
            refused: netns::Missing::of(bits & 1 == 0, bits & 2 == 0),
            refused_on_path: n[10],
            last_failed: netns::LastFailed {
                open: Some(netns::Reason::new(errno, &text)),
                ..netns::LastFailed::default()
            },
        };
        for ans in [Answer::Boxes { infos, more: false }, Answer::Refused(refusal), Answer::Exited(exit), Answer::Netns(counts)] {
            let bytes = answer(7, &ans, MAX_MESSAGE).unwrap();
            prop_assert_eq!(read_answer(&received(&bytes, 0), MAX_MESSAGE).unwrap().1, ans);
        }
    }

    /// HEX-SOCK-1: any bytes are a message or a typed refusal, never a panic, as a request or an
    /// answer, with any count of descriptors.
    #[test]
    fn any_bytes_are_a_message_or_refused(bytes in proptest::collection::vec(any::<u8>(), 0..512), fds in 0usize..5) {
        let _ = read_request(&received(&bytes, fds), MAX_MESSAGE);
        let _ = read_answer(&received(&bytes, fds), MAX_MESSAGE);
    }

    /// HEX-SOCK-1: bodies of random bytes behind a valid header, so every kind's decoder meets them, are a
    /// message or a typed refusal, never a panic.
    #[test]
    fn any_body_behind_a_header_is_read_or_refused(kind in prop_oneof![0u16..0x0013, 0x8000u16..0x800e], body in proptest::collection::vec(any::<u8>(), 0..256), fds in 0usize..4) {
        let mut bytes = header(kind, 3);
        bytes.extend_from_slice(&body);
        let _ = read_request(&received(&bytes, fds), MAX_MESSAGE);
        let _ = read_answer(&received(&bytes, fds), MAX_MESSAGE);
    }
}

/// HEX-SOCK-1: every single-bit flip of every request and answer is a message or a typed refusal,
/// never a panic.
#[test]
fn every_bit_flip_is_read_or_refused() {
    let messages: Vec<(Vec<u8>, usize)> = requests()
        .into_iter()
        .map(|(n, r, fds)| (request(n, &r, MAX_MESSAGE).unwrap(), fds))
        .chain(
            answers()
                .into_iter()
                .map(|(n, a)| (answer(n, &a, MAX_MESSAGE).unwrap(), 0)),
        )
        .collect();
    for (bytes, fds) in messages {
        for at in 0..bytes.len() {
            for bit in 0..8 {
                let mut flipped = bytes.clone();
                flipped[at] ^= 1 << bit;
                let _ = read_request(&received(&flipped, fds), MAX_MESSAGE);
                let _ = read_answer(&received(&flipped, fds), MAX_MESSAGE);
            }
        }
    }
}

/// HEX-GUEST-1 (docs/design/shards-guest.md §3): an exec naming stdin and stdout, sent with no
/// descriptors: expect it read whole where its stdio travels apart (a stream transport), and refused by
/// type where descriptors ride with the message.
#[test]
fn an_exec_whose_stdio_travels_apart_carries_no_descriptors() {
    let exec = Request::Call(Call::Exec {
        id: id("b"),
        program: Program::of(&["cat"]),
        streams: Streams {
            stdin: true,
            stdout: true,
            stderr: false,
        },
    });
    let bytes = request(7, &exec, MAX_MESSAGE).unwrap();
    let (h, read) = read_request_apart(&bytes, MAX_MESSAGE).unwrap();
    assert_eq!((h.id, read), (7, exec));
    assert!(matches!(
        read_request(&received(&bytes, 0), MAX_MESSAGE),
        Err(Bad {
            fault: Fault::Descriptors {
                want: 2,
                got: 0,
                ..
            },
            ..
        })
    ));
}

/// CP-T7.4: scheduling shares descriptor validation and message admission with Runtime calls.
#[test]
fn scheduler_control_cannot_carry_a_descriptor_or_exceed_the_wire_bound() {
    let request_value = Request::Call(Call::Schedule(Box::new(crate::schedule::Command::Retry)));
    let bytes = request(47, &request_value, MAX_MESSAGE).unwrap();
    assert_eq!(
        read_request_apart(&bytes, MAX_MESSAGE).unwrap().1,
        request_value
    );
    let received = Received {
        bytes: &bytes,
        size: bytes.len(),
        descriptors: 1,
        descriptors_cut: false,
    };
    assert!(matches!(
        read_request(&received, MAX_MESSAGE).unwrap_err().fault,
        Fault::Descriptors { .. }
    ));
    assert!(request(47, &request_value, HEADER_BYTES).is_err());
}

fn route_wire_key() -> BackingKey {
    BackingKey {
        pool: [0xa7; 32],
        generation: [0x6d; 16],
    }
}

fn route_wire_shared_key() -> BackingKey {
    BackingKey {
        pool: [0x19; 32],
        generation: [0x2a; 16],
    }
}

fn route_wire_shared_backing(phase: BackingPhase) -> BackingOffer {
    BackingOffer {
        guest: Default::default(),
        key: route_wire_shared_key(),
        cost: route_wire_cost(),
        phase,
        devices: Vec::new(),
    }
}

fn route_wire_cost() -> RuntimeCost {
    let fixed = CapacityResources {
        cpu_milli: 1000,
        mem_bytes: 4096,
        disk_bytes: 4096,
    };
    RuntimeCost {
        id: [0x31; 32],
        floor: fixed,
        ceiling: fixed,
        quantum: CapacityResources {
            cpu_milli: 1,
            mem_bytes: 1,
            disk_bytes: 1,
        },
        guest_overhead: CapacityResources::default(),
        overhead: CapacityResources {
            cpu_milli: 100,
            mem_bytes: 512,
            disk_bytes: 0,
        },
    }
}

fn route_wire_backing(phase: BackingPhase) -> BackingOffer {
    BackingOffer {
        guest: Default::default(),
        key: route_wire_key(),
        cost: route_wire_cost(),
        phase,
        // Administrator claims are not a measured catalog. Neither identity need appear there.
        devices: vec![
            DeviceId {
                kind: DeviceKind::Gpu,
                key: 99,
            },
            DeviceId {
                kind: DeviceKind::Gpu,
                key: u64::MAX,
            },
        ],
    }
}

fn route_wire_grant() -> BackingGrant {
    let offer = route_wire_backing(BackingPhase::Prepared);
    BackingGrant {
        key: offer.key,
        allocation: offer.allocation().unwrap(),
        devices: offer.devices,
    }
}

fn route_wire_route(execution: Execution) -> RouteOffer {
    RouteOffer {
        key: RouteKey {
            class: match execution {
                Execution::Firecracker => Class::Firecracker,
                _ => Class::Oci,
            },
            image: ImagePlatform {
                os: ImageOs::Linux,
                arch: ImageArch::Arm64,
            },
            execution,
            gpu: match execution {
                Execution::ShardsDedicated => GpuEligibility::Required,
                Execution::ShardsShared | Execution::NodeVm => GpuEligibility::None,
                _ => GpuEligibility::Any,
            },
        },
        cost: match execution {
            Execution::Firecracker | Execution::ShardsDedicated => Some(route_wire_cost()),
            _ => None,
        },
        backing: match execution {
            Execution::ShardsShared => Some(route_wire_shared_key()),
            Execution::ShardsDedicated => Some(route_wire_key()),
            Execution::Native | Execution::Firecracker | Execution::NodeVm => None,
        },
    }
}

fn route_wire_legacy_capacity() -> Capacity {
    let mut capacity = device_capacity(vec![
        DeviceInfo {
            id: DeviceId {
                kind: DeviceKind::Gpu,
                key: 4,
            },
            model: "private-model-first".into(),
            size: 8192,
            holder: None,
        },
        DeviceInfo {
            id: DeviceId {
                kind: DeviceKind::Gpu,
                key: 3,
            },
            model: "private-model-second".into(),
            size: 8192,
            holder: None,
        },
    ]);
    capacity.total = CapacityResources {
        cpu_milli: 8000,
        mem_bytes: 1 << 20,
        disk_bytes: 1 << 20,
    };
    capacity.free = capacity.total;
    capacity
}

fn route_wire_capacity() -> Capacity {
    let mut capacity = route_wire_legacy_capacity();
    capacity.routes = vec![
        route_wire_route(Execution::ShardsShared),
        route_wire_route(Execution::ShardsDedicated),
        route_wire_route(Execution::Firecracker),
    ];
    capacity.device_routes = vec![0b110, 0b100];
    capacity.backings = vec![
        route_wire_backing(BackingPhase::Live),
        route_wire_shared_backing(BackingPhase::Live),
    ];
    capacity
}

/// Joint plan step 14 (C7; ROOT, 2026-11-13): a shared backing's guest overhead, as its runtime measured it, crosses
/// to the scheduler on its offer: per dimension, none while cold. Do answer a capacity whose shared backing's guest is
/// measured at 0.25 CPU and 256 MiB, its disk unmeasured, and read it back. Expect it whole.
#[test]
fn w14_a_shared_backings_measured_guest_crosses_on_its_offer() {
    let mut capacity = route_wire_capacity();
    for backing in &mut capacity.backings {
        backing.guest = LearnedOverhead {
            cpu_milli: Some(250),
            mem_bytes: Some(256 << 20),
            disk_bytes: None,
        };
    }
    let expected = Answer::Capacity(Some(capacity));
    let bytes = answer(209, &expected, MAX_MESSAGE).unwrap();
    assert_eq!(
        read_answer(&received(&bytes, 0), MAX_MESSAGE).unwrap().1,
        expected
    );
}

// Use the real old prefix and emit chosen new fields without the capacity validator, so invalid
// cross-field inputs actually reach the public decoder rather than failing in a fixture builder.
fn route_wire_capacity_bytes(
    routes: &[RouteOffer],
    masks: &[u64],
    backings: &[BackingOffer],
) -> Vec<u8> {
    let mut prefix = answer(
        201,
        &Answer::Capacity(Some(route_wire_legacy_capacity())),
        MAX_MESSAGE,
    )
    .unwrap();
    // Its three route lists and its unavailable devices' count, all empty, then no learned use (joint plan step 14),
    // its machine's time unread (C7), no node VM's count (joint plan step 39), no learner's why (A-3) and its boxes' CPU
    // time unread (A-8), end it.
    prefix.truncate(prefix.len() - 4 * size_of::<u32>() - 1 - 2 - size_of::<u32>() - 1 - 1);
    let mut w = Writer::new(MAX_MESSAGE);
    w.raw(&prefix);
    w.u32(u32::try_from(routes.len()).unwrap());
    for route in routes {
        put_route_offer(&mut w, *route).unwrap();
    }
    w.u32(u32::try_from(masks.len()).unwrap());
    for mask in masks {
        w.u64(*mask);
    }
    w.u32(u32::try_from(backings.len()).unwrap());
    for backing in backings {
        put_backing_offer(&mut w, backing).unwrap();
    }
    w.u32(0);
    w.u8(0);
    // Its machine's time unread (joint plan step 14, C7), then no node VM's receipt (joint plan step 39), then no
    // learner's why (A-3), then its boxes' CPU time unread (A-8).
    w.u8(0);
    w.u8(0);
    w.u32(0);
    w.u8(0);
    w.u8(0);
    w.done().unwrap()
}

fn route_wire_assert_redacted(fault: &Fault) {
    let text = format!("{fault:?}");
    for private in [
        "private-model-first",
        "private-model-second",
        "a7a7a7a7",
        "6d6d6d6d",
    ] {
        assert!(!text.contains(private), "{text}");
    }
}

/// The new tags and every backing phase carry the exact canonical generation and physical claims.
#[test]
fn route_wire_backing_calls_and_all_phases_roundtrip() {
    for (tag, expected) in [
        (
            0x0014u16,
            Request::Call(Call::InitializeBacking(triomphe::Arc::new(
                route_wire_grant(),
            ))),
        ),
        (
            0x0015,
            Request::Call(Call::ReleaseBacking(route_wire_key())),
        ),
        (0x0016, Request::Call(Call::BackingUsage(route_wire_key()))),
    ] {
        let bytes = request(201, &expected, MAX_MESSAGE).unwrap();
        assert_eq!(&bytes[12..14], &tag.to_le_bytes());
        assert_eq!(&bytes[HEADER_BYTES..HEADER_BYTES + 32], &[0xa7; 32]);
        assert_eq!(
            &bytes[HEADER_BYTES + 32..HEADER_BYTES + BACKING_KEY_BYTES],
            &[0x6d; 16]
        );
        if tag != 0x0014 {
            assert_eq!(bytes.len(), HEADER_BYTES + BACKING_KEY_BYTES);
        }
        assert_eq!(
            read_request(&received(&bytes, 0), MAX_MESSAGE).unwrap().1,
            expected
        );
        assert_eq!(read_request_apart(&bytes, MAX_MESSAGE).unwrap().1, expected);
        assert!(matches!(
            read_request(&received(&bytes, 1), MAX_MESSAGE)
                .unwrap_err()
                .fault,
            Fault::Descriptors {
                want: 0,
                got: 1,
                ..
            }
        ));
    }
    for (phase, tag) in [
        (BackingPhase::Prepared, 0),
        (BackingPhase::Granted, 1),
        (BackingPhase::Live, 2),
        (BackingPhase::Draining, 3),
        (BackingPhase::Released, 4),
    ] {
        let expected = Answer::BackingOffer(route_wire_backing(phase));
        let bytes = answer(202, &expected, MAX_MESSAGE).unwrap();
        assert_eq!(&bytes[12..14], &0x800fu16.to_le_bytes());
        assert_eq!(&bytes[HEADER_BYTES..HEADER_BYTES + 32], &[0xa7; 32]);
        assert_eq!(
            &bytes[HEADER_BYTES + 32..HEADER_BYTES + BACKING_KEY_BYTES],
            &[0x6d; 16]
        );
        assert_eq!(bytes[HEADER_BYTES + BACKING_KEY_BYTES + COST_BYTES], tag);
        assert_eq!(
            read_answer(&received(&bytes, 0), MAX_MESSAGE).unwrap().1,
            expected
        );
        assert!(matches!(
            read_answer(&received(&bytes, 1), MAX_MESSAGE),
            Err(Fault::Descriptors { .. })
        ));
    }
    let expected = Answer::Usage(Usage {
        memory_bytes: 4096,
        memory_peak_bytes: 8192,
        oom_kills: 0,
        pids: 2,
        pids_refused: 0,
        cpu_usec: 77,
        io_read_bytes: 17,
        io_write_bytes: 19,
        guest: None,
        started_from: None,
    });
    let bytes = answer(203, &expected, MAX_MESSAGE).unwrap();
    assert_eq!(&bytes[12..14], &0x8005u16.to_le_bytes());
    // The counters, then guest time's option byte and the microVM's start's, appended (joint plan step 12): none here.
    assert_eq!(bytes.len(), HEADER_BYTES + size_of::<[u64; 8]>() + 2);
    assert_eq!(
        read_answer(&received(&bytes, 0), MAX_MESSAGE).unwrap().1,
        expected
    );
}

/// Fixed enum tags describe the original workload/image and initialized execution independently.
#[test]
fn route_wire_key_enum_tags_are_explicit_and_roundtrip() {
    for (class, os, arch, execution, gpu, expected) in [
        (
            Class::Process,
            ImageOs::Macos,
            ImageArch::Arm64,
            Execution::Native,
            GpuEligibility::None,
            [0, 2, 1, 0, 1],
        ),
        (
            Class::Process,
            ImageOs::Windows,
            ImageArch::Amd64,
            Execution::Native,
            GpuEligibility::None,
            [0, 1, 0, 0, 1],
        ),
        (
            Class::Oci,
            ImageOs::Windows,
            ImageArch::Arm64,
            Execution::Native,
            GpuEligibility::Any,
            [1, 1, 1, 0, 0],
        ),
        (
            Class::Oci,
            ImageOs::Linux,
            ImageArch::Amd64,
            Execution::ShardsShared,
            GpuEligibility::None,
            [1, 0, 0, 1, 1],
        ),
        (
            Class::Oci,
            ImageOs::Linux,
            ImageArch::Arm64,
            Execution::ShardsDedicated,
            GpuEligibility::Required,
            [1, 0, 1, 2, 2],
        ),
        (
            Class::Firecracker,
            ImageOs::Linux,
            ImageArch::Amd64,
            Execution::Firecracker,
            GpuEligibility::Required,
            [2, 0, 0, 3, 2],
        ),
        (
            Class::Shards,
            ImageOs::Linux,
            ImageArch::Arm64,
            Execution::ShardsDedicated,
            GpuEligibility::None,
            [3, 0, 1, 2, 1],
        ),
    ] {
        let key = RouteKey {
            class,
            image: ImagePlatform { os, arch },
            execution,
            gpu,
        };
        let mut w = Writer::new(ROUTE_KEY_BYTES);
        put_route_key(&mut w, key).unwrap();
        let bytes = w.done().unwrap();
        assert_eq!(bytes, expected);
        assert_eq!(route_key(&mut Reader::new(&bytes)).unwrap(), key);
        for end in 0..bytes.len() {
            assert!(matches!(
                route_key(&mut Reader::new(&bytes[..end])),
                Err(Fault::Truncated { .. })
            ));
        }
    }
}

/// Masks remain joined by position to the original unsorted physical list. Provisioning does not
/// manufacture measured entries, and OCI shared/dedicated routes retain the original OCI class.
#[test]
fn route_wire_capacity_preserves_routes_masks_and_unmeasured_backings() {
    let expected = Answer::Capacity(Some(route_wire_capacity()));
    let bytes = answer(203, &expected, MAX_MESSAGE).unwrap();
    let actual = read_answer(&received(&bytes, 0), MAX_MESSAGE).unwrap().1;
    assert_eq!(actual, expected);
    let Answer::Capacity(Some(capacity)) = actual else {
        panic!("capacity")
    };
    assert_eq!(
        capacity
            .devices
            .iter()
            .map(|d| d.id.key)
            .collect::<Vec<_>>(),
        [4, 3]
    );
    assert_eq!(capacity.device_routes, [6, 4]);
    assert_eq!(capacity.routes[0].key.class, Class::Oci);
    assert_eq!(capacity.routes[1].key.class, Class::Oci);
    assert_eq!(capacity.backings.len(), 2);
    assert!(capacity.backings[1].devices.is_empty());
    assert_eq!(capacity.routes[0].backing, Some(capacity.backings[1].key));
    assert_eq!(capacity.routes[1].backing, Some(capacity.backings[0].key));
    assert_eq!(capacity.backings[0].devices[1].key, u64::MAX);
    assert!(
        !capacity
            .devices
            .iter()
            .any(|d| d.id == capacity.backings[0].devices[1])
    );
    let legacy = Answer::Capacity(Some(route_wire_legacy_capacity()));
    let bytes = answer(204, &legacy, MAX_MESSAGE).unwrap();
    assert_eq!(
        read_answer(&received(&bytes, 0), MAX_MESSAGE).unwrap().1,
        legacy
    );
}

/// Both tails preserve the original class, caps, selected IDs and allocation beside the route.
#[test]
fn route_wire_spec_and_info_route_tails_roundtrip() {
    for execution in [
        Execution::Native,
        Execution::ShardsShared,
        Execution::ShardsDedicated,
        Execution::Firecracker,
    ] {
        let route = route_wire_route(execution);
        let mut spec = oci_spec();
        if execution == Execution::Firecracker {
            let ClassSpec::Oci(oci) = spec.class else {
                panic!("OCI fixture")
            };
            spec.class = ClassSpec::Firecracker(oci);
        }
        if route.key.gpu == GpuEligibility::Required {
            spec.devices.push(DeviceId {
                kind: DeviceKind::Gpu,
                key: u64::MAX,
            });
        }
        if let Some(mut cost) = route.cost {
            // Routed specs pass Bounds::check as well as the receipt's numeric codec.
            cost.floor.mem_bytes = hex_api::MIN_MEMORY;
            cost.ceiling.mem_bytes = hex_api::MIN_MEMORY;
            spec.bounds.cpu_max_milli = Some(1000);
            spec.bounds.memory_max = Some(hex_api::MIN_MEMORY);
            spec.allocation = Some(cost.allocate(CapacityResources::default()).unwrap());
        }
        let receipt = RouteReceipt {
            key: route.key,
            backing: route.backing,
        };
        spec.route = Some(receipt);
        spec.check().expect("valid routed spec baseline");
        let expected = Request::Call(Call::Create(Box::new(spec.clone())));
        let bytes = request(205, &expected, MAX_MESSAGE).unwrap();
        assert_eq!(
            read_request(&received(&bytes, 0), MAX_MESSAGE).unwrap().1,
            expected
        );

        let mut plain = spec.clone();
        plain.route = None;
        let plain = request(
            205,
            &Request::Call(Call::Create(Box::new(plain))),
            MAX_MESSAGE,
        )
        .unwrap();
        // The route's option, then the band (0 here), end a spec (joint plan step 14).
        assert_eq!(plain.last(), Some(&0));
        let route_at = plain.len() - 2;
        assert_eq!(plain[route_at], 0);
        assert_eq!(&plain[..route_at], &bytes[..route_at]);
        assert_eq!(bytes[route_at], 1);

        let expected = Answer::Info(BoxInfo {
            class: spec.class.class(),
            bounds: spec.bounds,
            allocation: spec.allocation,
            route: Some(receipt),
            ..info()
        });
        let bytes = answer(206, &expected, MAX_MESSAGE).unwrap();
        assert_eq!(
            read_answer(&received(&bytes, 0), MAX_MESSAGE).unwrap().1,
            expected
        );
    }
}

/// Backing failures are appended after Lifecycle, carrying exact generation and typed uncertainty.
#[test]
fn route_wire_backing_refusals_preserve_key_and_cause() {
    for (cause, tag) in [
        (LifecycleCause::Pending, 0),
        (LifecycleCause::Uncertain, 1),
        (LifecycleCause::Busy, 2),
    ] {
        let expected = Answer::Refused(Refusal::Backing {
            key: route_wire_key(),
            cause,
            why: None,
        });
        let bytes = answer(207, &expected, MAX_MESSAGE).unwrap();
        assert_eq!(bytes.len(), HEADER_BYTES + 1 + BACKING_KEY_BYTES + 1 + 1);
        assert_eq!(bytes[HEADER_BYTES], 10);
        assert_eq!(bytes[bytes.len() - 2], tag);
        assert_eq!(bytes.last(), Some(&0), "no reason, a byte 0");
        assert_eq!(
            read_answer(&received(&bytes, 0), MAX_MESSAGE).unwrap().1,
            expected
        );
    }
}

/// Cross-field malformed masks/references must reach the decoder, not merely a fixture validator.
#[test]
fn route_wire_hostile_masks_references_and_duplicate_pools_refuse() {
    let shared = route_wire_route(Execution::ShardsShared);
    let native = route_wire_route(Execution::Native);
    let backing = route_wire_shared_backing(BackingPhase::Live);
    for masks in [
        vec![],
        vec![1],
        vec![1, 1, 1],
        vec![2, 0],
        vec![0, u64::MAX],
    ] {
        let bytes = route_wire_capacity_bytes(&[native], &masks, &[]);
        let fault = read_answer(&received(&bytes, 0), MAX_MESSAGE).unwrap_err();
        assert!(matches!(
            fault,
            Fault::Value {
                what: "capacity",
                ..
            }
        ));
        route_wire_assert_redacted(&fault);
    }
    for bytes in [
        route_wire_capacity_bytes(&[shared], &[0, 0], &[]),
        route_wire_capacity_bytes(&[native, native], &[1, 1], &[]),
        route_wire_capacity_bytes(&[], &[0, 0], &[]),
        route_wire_capacity_bytes(&[], &[], &[backing.try_clone().unwrap()]),
    ] {
        let fault = read_answer(&received(&bytes, 0), MAX_MESSAGE).unwrap_err();
        assert!(matches!(
            fault,
            Fault::Value {
                what: "capacity",
                ..
            }
        ));
        route_wire_assert_redacted(&fault);
    }
    let mut transplanted = backing.try_clone().unwrap();
    transplanted.key.generation[0] ^= 1;
    let bytes = route_wire_capacity_bytes(&[shared], &[0, 0], &[transplanted]);
    assert!(matches!(
        read_answer(&received(&bytes, 0), MAX_MESSAGE),
        Err(Fault::Value {
            what: "capacity",
            ..
        })
    ));
    let mut competing = backing.try_clone().unwrap();
    competing.key.generation[0] ^= 1;
    let bytes = route_wire_capacity_bytes(&[shared], &[0, 0], &[backing, competing]);
    assert!(matches!(
        read_answer(&received(&bytes, 0), MAX_MESSAGE),
        Err(Fault::Value {
            what: "capacity",
            ..
        })
    ));

    let mutations: [fn(&mut Capacity); 3] = [
        |c: &mut Capacity| c.device_routes[0] = 1 << c.routes.len(),
        |c: &mut Capacity| {
            c.routes[0].backing.as_mut().unwrap().generation[0] ^= 1;
        },
        |c: &mut Capacity| {
            c.backings.push(c.backings[0].try_clone().unwrap());
        },
    ];
    for mutate in mutations {
        let mut invalid = route_wire_capacity();
        mutate(&mut invalid);
        assert!(matches!(
            answer(208, &Answer::Capacity(Some(invalid)), MAX_MESSAGE),
            Err(Fault::Value {
                what: "capacity",
                ..
            })
        ));
    }
}

/// All new list counts are bounded before element parsing or reserve, including minimum-width
/// bodies rather than the old one-byte-per-item approximation.
#[test]
fn route_wire_hostile_counts_and_minimum_bytes_refuse_before_parse() {
    for (what, minimum, maximum) in [
        ("routes", ROUTE_OFFER_MIN_BYTES, MAX_RUNTIME_ROUTES),
        ("backings", BACKING_OFFER_MIN_BYTES, MAX_RUNTIME_ROUTES),
        ("devices", size_of::<u64>(), MAX_DEVICES),
    ] {
        for (count, value_error) in [
            (1u32, false),
            (u32::try_from(maximum + 1).unwrap(), true),
            (u32::MAX, true),
        ] {
            let bytes = count.to_le_bytes();
            let mut reader = Reader::new(&bytes);
            let mut parsed = false;
            let result = reader.bounded_list(minimum, maximum, what, |_| {
                parsed = true;
                Ok(())
            });
            assert!(!parsed);
            match result.unwrap_err() {
                Fault::Value { what: got, .. } if value_error => assert_eq!(got, what),
                Fault::Truncated { .. } if !value_error => {}
                other => panic!("{other:?}"),
            }
        }
        let mut bytes = 1u32.to_le_bytes().to_vec();
        bytes.resize(4 + minimum - 1, 0);
        let mut parsed = false;
        let result = Reader::new(&bytes).bounded_list(minimum, maximum, what, |_| {
            parsed = true;
            Ok(())
        });
        assert!(matches!(result, Err(Fault::Truncated { .. })));
        assert!(!parsed);
    }

    // Route/backing count bounds are also enforced through the actual Capacity decoder.
    let mut prefix = answer(
        209,
        &Answer::Capacity(Some(route_wire_legacy_capacity())),
        MAX_MESSAGE,
    )
    .unwrap();
    // Its three route lists and its unavailable devices' count, all empty, then no learned use (joint plan step 14),
    // its machine's time unread (C7), no node VM's count (joint plan step 39), no learner's why (A-3) and its boxes' CPU
    // time unread (A-8), end it.
    prefix.truncate(prefix.len() - 4 * size_of::<u32>() - 1 - 2 - size_of::<u32>() - 1 - 1);
    for count in [1u32, 65, u32::MAX] {
        let mut bytes = prefix.clone();
        bytes.extend(count.to_le_bytes());
        assert!(read_answer(&received(&bytes, 0), MAX_MESSAGE).is_err());
        let mut bytes = prefix.clone();
        bytes.extend(0u32.to_le_bytes()); // no routes
        bytes.extend(0u32.to_le_bytes()); // no masks
        bytes.extend(count.to_le_bytes());
        assert!(read_answer(&received(&bytes, 0), MAX_MESSAGE).is_err());
    }
}

/// Unknown enum tags and semantically incompatible route shapes fail without echoing key bytes.
#[test]
fn route_wire_hostile_route_and_backing_tags_are_redacted() {
    for at in 0..ROUTE_KEY_BYTES {
        let mut bytes = [1, 0, 1, 0, 0];
        bytes[at] = 255;
        let fault = route_key(&mut Reader::new(&bytes)).unwrap_err();
        assert!(matches!(fault, Fault::Value { .. }));
        route_wire_assert_redacted(&fault);
    }
    for bytes in [
        [0, 0, 0, 0, 2], // process cannot require a GPU
        [1, 2, 1, 0, 0], // native OCI cannot claim a macOS image
        [2, 1, 0, 3, 1], // Firecracker requires Linux
        [1, 0, 0, 1, 2], // shared OCI cannot require a GPU
        [1, 0, 0, 2, 1], // dedicated OCI requires selected GPUs
    ] {
        assert!(matches!(
            route_key(&mut Reader::new(&bytes)),
            Err(Fault::Value { what: "route", .. })
        ));
    }
    for tag in [5, 255] {
        let mut bytes = answer(
            210,
            &Answer::BackingOffer(route_wire_backing(BackingPhase::Live)),
            MAX_MESSAGE,
        )
        .unwrap();
        bytes[HEADER_BYTES + BACKING_KEY_BYTES + COST_BYTES] = tag;
        let fault = read_answer(&received(&bytes, 0), MAX_MESSAGE).unwrap_err();
        assert!(matches!(
            fault,
            Fault::Value {
                what: "backing.phase",
                ..
            }
        ));
        route_wire_assert_redacted(&fault);
    }
    let mut bytes = answer(
        210,
        &Answer::Refused(Refusal::Backing {
            key: route_wire_key(),
            cause: LifecycleCause::Uncertain,
            why: None,
        }),
        MAX_MESSAGE,
    )
    .unwrap();
    // 3 is Retired (joint plan step 14), 4 NodeVmEnded (joint plan step 39, A-6); 5 is no cause. The cause is the byte
    // before its reason's option tag.
    let cause = bytes.len() - 2;
    bytes[cause] = 5;
    let fault = read_answer(&received(&bytes, 0), MAX_MESSAGE).unwrap_err();
    assert!(matches!(
        fault,
        Fault::Value {
            what: "backing.cause",
            ..
        }
    ));
    route_wire_assert_redacted(&fault);
}

/// Backing claims use the shared physical parser and canonical sorted-unique invariant.
#[test]
fn route_wire_backing_claims_reject_duplicates_order_bad_ids_and_counts() {
    for grant in [false, true] {
        let mut bytes = if grant {
            request(
                211,
                &Request::Call(Call::InitializeBacking(triomphe::Arc::new(
                    route_wire_grant(),
                ))),
                MAX_MESSAGE,
            )
            .unwrap()
        } else {
            answer(
                211,
                &Answer::BackingOffer(route_wire_backing(BackingPhase::Live)),
                MAX_MESSAGE,
            )
            .unwrap()
        };
        // Where its claims begin: after its count, an offer's guest following them (joint plan step 14, C7).
        let minimum = if grant {
            BACKING_GRANT_MIN_BYTES
        } else {
            BACKING_OFFER_MIN_BYTES - BACKING_OFFER_GUEST_MIN_BYTES
        };
        let first = HEADER_BYTES + minimum;
        let original = bytes.clone();
        let count_at = first - size_of::<u32>();
        for count in [
            1u32,
            u32::try_from(MAX_DEVICES).unwrap(),
            u32::try_from(MAX_DEVICES + 1).unwrap(),
            u32::MAX,
        ] {
            bytes = original[..count_at].to_vec();
            bytes.extend(count.to_le_bytes());
            let result = if grant {
                read_request(&received(&bytes, 0), MAX_MESSAGE)
                    .map(|_| ())
                    .map_err(|bad| bad.fault)
            } else {
                read_answer(&received(&bytes, 0), MAX_MESSAGE).map(|_| ())
            };
            assert!(result.is_err());
        }
        for kind in [255, 3] {
            bytes = original.clone();
            bytes[first] = kind; // kind=Net with key99 is valid; make its width invalid below.
            if kind == 3 {
                bytes[first + 1..first + 9].copy_from_slice(&u64::MAX.to_le_bytes());
            }
            let fault = if grant {
                read_request(&received(&bytes, 0), MAX_MESSAGE)
                    .unwrap_err()
                    .fault
            } else {
                read_answer(&received(&bytes, 0), MAX_MESSAGE).unwrap_err()
            };
            assert!(matches!(fault, Fault::Value { .. }));
            route_wire_assert_redacted(&fault);
        }
        for reversed in [false, true] {
            bytes = original.clone();
            let mut ids = bytes[first..first + 18].to_vec();
            if reversed {
                ids.rotate_left(9);
            } else {
                ids.copy_within(0..9, 9);
            }
            bytes[first..first + 18].copy_from_slice(&ids);
            let fault = if grant {
                read_request(&received(&bytes, 0), MAX_MESSAGE)
                    .unwrap_err()
                    .fault
            } else {
                read_answer(&received(&bytes, 0), MAX_MESSAGE).unwrap_err()
            };
            assert!(matches!(
                fault,
                Fault::Value {
                    what: "backing",
                    ..
                }
            ));
        }
    }
}

/// Dedicated routes cannot smuggle a changed original class, absent finite caps/allocation, or
/// GPU-less required receipt; factual BoxInfo also cannot relabel the original class.
#[test]
fn route_wire_spec_and_info_inconsistent_route_receipts_refuse() {
    let route = route_wire_route(Execution::ShardsDedicated);
    let mut cost = route.cost.unwrap();
    cost.floor.mem_bytes = hex_api::MIN_MEMORY;
    cost.ceiling.mem_bytes = hex_api::MIN_MEMORY;
    let mut valid = oci_spec();
    valid.bounds.cpu_max_milli = Some(1000);
    valid.bounds.memory_max = Some(hex_api::MIN_MEMORY);
    valid.allocation = Some(cost.allocate(CapacityResources::default()).unwrap());
    valid.devices = vec![DeviceId {
        kind: DeviceKind::Gpu,
        key: 99,
    }];
    valid.route = Some(RouteReceipt {
        key: route.key,
        backing: route.backing,
    });
    valid.check().expect("valid dedicated spec baseline");
    let good = request(
        212,
        &Request::Call(Call::Create(Box::new(valid.clone()))),
        MAX_MESSAGE,
    )
    .unwrap();
    assert!(read_request(&received(&good, 0), MAX_MESSAGE).is_ok());

    for change in 0..4 {
        let mut bad = valid.clone();
        match change {
            0 => bad.allocation = None,
            1 => bad.bounds.memory_max = None,
            2 => bad.devices.clear(),
            _ => bad.route.as_mut().unwrap().key.class = Class::Shards,
        }
        assert!(matches!(
            request(
                212,
                &Request::Call(Call::Create(Box::new(bad))),
                MAX_MESSAGE
            ),
            Err(Fault::Value { what: "route", .. })
        ));
    }
    // Keep a valid encoded body and change only the committed original class.
    let mut bad = good;
    // The band's byte follows the route (joint plan step 14).
    let route_bytes = 1 + ROUTE_KEY_BYTES + 1 + BACKING_KEY_BYTES;
    let route_at = bad.len() - route_bytes - 1;
    assert_eq!(bad[route_at], 1);
    assert_eq!(bad[route_at + 1], class_tag(Class::Oci));
    assert_eq!(bad[route_at + 1 + ROUTE_KEY_BYTES], 1);
    bad[route_at + 1] = 3; // Shards/Dedicated is valid alone, but differs from original OCI.
    assert!(matches!(
        read_request(&received(&bad, 0), MAX_MESSAGE)
            .unwrap_err()
            .fault,
        Fault::Value { what: "route", .. }
    ));

    let bad_info = Answer::Info(BoxInfo {
        class: Class::Process,
        route: valid.route,
        ..info()
    });
    assert!(matches!(
        answer(212, &bad_info, MAX_MESSAGE),
        Err(Fault::Value { what: "route", .. })
    ));
    let valid_info = Answer::Info(BoxInfo {
        route: valid.route,
        ..info()
    });
    let mut bad = answer(212, &valid_info, MAX_MESSAGE).unwrap();
    assert_eq!(
        read_answer(&received(&bad, 0), MAX_MESSAGE).unwrap().1,
        valid_info
    );
    // The info's route comes before its empty grant and published lists' counts, its last resort's and its run's
    // words' none tags, its empty credential list's count and its agent-control endpoint's none tag.
    let route_at = bad.len()
        - NO_GRANTS
        - NO_END
        - NO_WORDS
        - NO_CREDENTIALS
        - NO_CONTROL
        - NO_WAKE
        - route_bytes;
    assert_eq!(bad[route_at], 1);
    assert_eq!(bad[route_at + 1], class_tag(Class::Oci));
    assert_eq!(bad[route_at + 1 + ROUTE_KEY_BYTES], 1);
    bad[route_at + 1] = 3;
    assert!(matches!(
        read_answer(&received(&bad, 0), MAX_MESSAGE),
        Err(Fault::Value { what: "route", .. })
    ));
}

/// Every new body is bounded exactly, fully consumed and refused at every truncation; this covers
/// no old suite and dispatches no native work.
#[test]
fn route_wire_new_messages_enforce_exact_bounds_truncation_and_trailing_bytes() {
    for expected in [
        Request::Call(Call::InitializeBacking(triomphe::Arc::new(
            route_wire_grant(),
        ))),
        Request::Call(Call::ReleaseBacking(route_wire_key())),
        Request::Call(Call::BackingUsage(route_wire_key())),
        Request::Call(Call::Create(Box::new({
            let mut spec = oci_spec();
            spec.route = Some(RouteReceipt {
                key: route_wire_route(Execution::ShardsShared).key,
                backing: Some(route_wire_shared_key()),
            });
            spec
        }))),
    ] {
        let bytes = request(213, &expected, MAX_MESSAGE).unwrap();
        assert_eq!(request(213, &expected, bytes.len()).unwrap(), bytes);
        assert!(matches!(
            request(213, &expected, bytes.len() - 1),
            Err(Fault::Oversized { .. })
        ));
        assert!(matches!(
            read_request(&received(&bytes, 0), bytes.len() - 1)
                .unwrap_err()
                .fault,
            Fault::Oversized { .. }
        ));
        for end in 0..bytes.len() {
            assert!(read_request(&received(&bytes[..end], 0), MAX_MESSAGE).is_err());
        }
        let mut trailing = bytes;
        trailing.push(0);
        assert!(read_request(&received(&trailing, 0), MAX_MESSAGE).is_err());
    }
    for expected in [
        Answer::BackingOffer(route_wire_backing(BackingPhase::Live)),
        Answer::Capacity(Some(route_wire_capacity())),
        Answer::Info(BoxInfo {
            route: Some(RouteReceipt {
                key: route_wire_route(Execution::ShardsShared).key,
                backing: Some(route_wire_shared_key()),
            }),
            ..info()
        }),
        Answer::Refused(Refusal::Backing {
            key: route_wire_key(),
            cause: LifecycleCause::Pending,
            why: None,
        }),
        Answer::Refused(Refusal::Backing {
            key: route_wire_key(),
            cause: LifecycleCause::Uncertain,
            why: Some("its stop's end wasn't seen".to_owned()),
        }),
    ] {
        let bytes = answer(214, &expected, MAX_MESSAGE).unwrap();
        assert_eq!(answer(214, &expected, bytes.len()).unwrap(), bytes);
        assert!(matches!(
            answer(214, &expected, bytes.len() - 1),
            Err(Fault::Oversized { .. })
        ));
        assert!(matches!(
            read_answer(&received(&bytes, 0), bytes.len() - 1),
            Err(Fault::Oversized { .. })
        ));
        for end in 0..bytes.len() {
            assert!(read_answer(&received(&bytes[..end], 0), MAX_MESSAGE).is_err());
        }
        let mut trailing = bytes;
        trailing.push(0);
        assert!(read_answer(&received(&trailing, 0), MAX_MESSAGE).is_err());
    }
}

/// A fixed backing grant cannot change its host charge or substitute a variable per-box profile.
#[test]
fn route_wire_fixed_backing_profiles_and_grants_are_revalidated() {
    let valid = route_wire_grant();
    for change in 0..3 {
        let mut grant = valid.try_clone().unwrap();
        match change {
            0 => grant.allocation.resources.cpu_milli += 1,
            1 => grant.allocation.cost.ceiling.mem_bytes += 1,
            _ => grant.allocation.resources.mem_bytes = 0,
        }
        let mut raw = Writer::new(MAX_MESSAGE);
        raw.header(kind::INITIALIZE_BACKING, 215);
        put_backing_key(&mut raw, grant.key).unwrap();
        put_resources(&mut raw, grant.allocation.resources);
        put_cost(&mut raw, grant.allocation.cost).unwrap();
        put_provisioning(&mut raw, &grant.devices).unwrap();
        let bytes = raw.done().unwrap();
        assert!(matches!(
            read_request(&received(&bytes, 0), MAX_MESSAGE)
                .unwrap_err()
                .fault,
            Fault::Value {
                what: "backing",
                ..
            }
        ));
        assert!(matches!(
            request(
                215,
                &Request::Call(Call::InitializeBacking(triomphe::Arc::new(grant))),
                MAX_MESSAGE
            ),
            Err(Fault::Value {
                what: "backing",
                ..
            })
        ));
    }
    let mut offer = route_wire_backing(BackingPhase::Prepared);
    offer.cost.ceiling.cpu_milli += 1;
    let mut raw = Writer::new(MAX_MESSAGE);
    raw.header(kind::BACKING_OFFER, 215);
    put_backing_key(&mut raw, offer.key).unwrap();
    put_cost(&mut raw, offer.cost).unwrap();
    raw.u8(0);
    put_provisioning(&mut raw, &offer.devices).unwrap();
    // Its guest, each dimension none (joint plan step 14, C7).
    for _ in 0..BACKING_OFFER_GUEST_MIN_BYTES {
        raw.u8(0);
    }
    let bytes = raw.done().unwrap();
    assert!(matches!(
        read_answer(&received(&bytes, 0), MAX_MESSAGE),
        Err(Fault::Value {
            what: "backing",
            ..
        })
    ));
    assert!(matches!(
        answer(215, &Answer::BackingOffer(offer), MAX_MESSAGE),
        Err(Fault::Value {
            what: "backing",
            ..
        })
    ));
}

/// The holder discriminant extends the old option without freeing or relabeling a pool claim.
#[test]
fn route_wire_holder_tags_keep_legacy_box_and_exact_backing_generation() {
    for (holder, expected) in [
        (None, vec![0]),
        (
            Some(DeviceHolder::Box(id("box"))),
            vec![1, 3, 0, 0, 0, b'b', b'o', b'x'],
        ),
        (Some(DeviceHolder::Backing(route_wire_key())), {
            let mut bytes = vec![2];
            bytes.extend([0xa7; 32]);
            bytes.extend([0x6d; 16]);
            bytes
        }),
    ] {
        let mut w = Writer::new(MAX_MESSAGE);
        put_device_holder(&mut w, holder.as_ref()).unwrap();
        let bytes = w.done().unwrap();
        assert_eq!(bytes, expected);
        assert_eq!(device_holder(&mut Reader::new(&bytes)).unwrap(), holder);
    }
    let mut capacity = route_wire_capacity();
    capacity.backings[0].devices = vec![capacity.devices[0].id];
    capacity.devices[0].holder = Some(DeviceHolder::Backing(route_wire_key()));
    capacity.devices[1].holder = Some(DeviceHolder::Box(id("native-box")));
    let expected = Answer::Capacity(Some(capacity));
    let bytes = answer(216, &expected, MAX_MESSAGE).unwrap();
    let actual = read_answer(&received(&bytes, 0), MAX_MESSAGE).unwrap().1;
    assert_eq!(actual, expected);
    let Answer::Capacity(Some(actual)) = actual else {
        panic!("capacity")
    };
    assert!(
        matches!(actual.devices[0].holder, Some(DeviceHolder::Backing(key)) if key == route_wire_key())
    );
    assert!(
        matches!(&actual.devices[1].holder, Some(DeviceHolder::Box(key)) if key.as_str() == "native-box")
    );
}

/// Missing or malformed owner bytes never become None; diagnostics expose no private BoxId.
#[test]
fn route_wire_holder_unknown_tags_partial_keys_and_bad_boxes_refuse() {
    for tag in [3, 255] {
        let fault = device_holder(&mut Reader::new(&[tag])).unwrap_err();
        assert!(matches!(
            fault,
            Fault::Value {
                what: "devices.holder",
                ..
            }
        ));
    }
    let mut backing = vec![2];
    backing.extend([0xa7; 32]);
    backing.extend([0x6d; 16]);
    for end in 0..backing.len() {
        assert!(matches!(
            device_holder(&mut Reader::new(&backing[..end])),
            Err(Fault::Truncated { .. })
        ));
    }
    for name in ["../private-holder", "private/holder"] {
        let mut raw = Writer::new(MAX_MESSAGE);
        raw.u8(1);
        raw.string(name).unwrap();
        let bytes = raw.done().unwrap();
        let fault = device_holder(&mut Reader::new(&bytes)).unwrap_err();
        assert!(matches!(
            fault,
            Fault::Value {
                what: "devices.holder",
                ..
            }
        ));
        assert!(!format!("{fault:?}").contains("private"));
    }
    let mut raw = Writer::new(MAX_MESSAGE);
    raw.u8(1);
    raw.bytes(&[0xff]).unwrap();
    let bytes = raw.done().unwrap();
    assert!(matches!(
        device_holder(&mut Reader::new(&bytes)),
        Err(Fault::Value {
            what: "devices.holder",
            ..
        })
    ));
}

/// The factual provider remains holder-free even after catalog backing ownership becomes typed.
#[test]
fn route_wire_physical_reports_forbid_both_box_and_backing_holders() {
    for holder in [
        DeviceHolder::Box(id("private-telemetry-box")),
        DeviceHolder::Backing(route_wire_key()),
    ] {
        let mut report = example_physical_report();
        report.devices[0].info.holder = Some(holder);
        let fault = answer(217, &Answer::PhysicalReport(report), MAX_MESSAGE).unwrap_err();
        assert!(matches!(
            fault,
            Fault::Value {
                what: "physical report",
                ..
            }
        ));
        route_wire_assert_redacted(&fault);
        assert!(!format!("{fault:?}").contains("private-telemetry-box"));
    }
}

/// AC-T7.4.1: retained snapshots use exactly the ordinary current Answer bytes,
/// including every refusal variant, canonical id zero and the existing exit tags.
#[test]
fn snapshot_refusal_and_exit_match_canonical_answer_bytes() {
    let mut inputs = refusals();
    for cause in [
        LifecycleCause::Pending,
        LifecycleCause::Busy,
        LifecycleCause::Uncertain,
    ] {
        inputs.push(Refusal::Backing {
            key: route_wire_key(),
            cause,
            why: None,
        });
    }
    inputs.push(Refusal::Backing {
        key: route_wire_key(),
        cause: LifecycleCause::Uncertain,
        why: Some("its record names another generation".to_owned()),
    });
    for refusal in inputs {
        let ordinary = Answer::Refused(refusal);
        let expected = answer(0, &ordinary, MAX_MESSAGE).unwrap();
        let Answer::Refused(refusal) = &ordinary else {
            panic!("owned refusal fixture changed its variant");
        };
        let snapshot = snapshot_refusal(refusal, MAX_MESSAGE).unwrap();
        assert_eq!(snapshot, expected);
        let mut header = Reader::new(&snapshot);
        assert_eq!(header.header().unwrap(), (IDENTITY, kind::REFUSED, 0));
    }
    for exit in [
        Exit::Code(0),
        Exit::Code(-7),
        Exit::Code(i32::MAX),
        Exit::Signal(9),
        Exit::Unknown,
    ] {
        let expected = answer(0, &Answer::Exited(exit), MAX_MESSAGE).unwrap();
        assert_eq!(snapshot_exit(exit, MAX_MESSAGE).unwrap(), expected);
    }
}

/// AC-T7.4.1: every shorter bound refuses before snapshot allocation; in
/// particular, an Exhausted usize trailer cannot grow beyond a fitting string.
#[test]
fn snapshot_bounds_include_scalar_trailers_before_allocation() {
    let mut inputs = refusals();
    inputs.push(Refusal::Backing {
        key: route_wire_key(),
        cause: LifecycleCause::Uncertain,
        why: None,
    });
    inputs.push(Refusal::Backing {
        key: route_wire_key(),
        cause: LifecycleCause::Uncertain,
        why: Some("its receipt is missing".to_owned()),
    });
    for refusal in inputs {
        let ordinary = Answer::Refused(refusal);
        let expected = answer(0, &ordinary, MAX_MESSAGE).unwrap();
        let Answer::Refused(refusal) = &ordinary else {
            panic!("owned refusal fixture changed its variant");
        };
        assert_eq!(snapshot_refusal(refusal, expected.len()).unwrap(), expected);
        for bound in 0..expected.len() {
            assert!(matches!(
                snapshot_refusal(refusal, bound),
                Err(Fault::Oversized { .. })
            ));
        }
    }
    for exit in [Exit::Code(7), Exit::Signal(9), Exit::Unknown] {
        let expected = answer(0, &Answer::Exited(exit), MAX_MESSAGE).unwrap();
        for bound in 0..expected.len() {
            assert!(matches!(
                snapshot_exit(exit, bound),
                Err(Fault::Oversized { .. })
            ));
        }
        assert_eq!(snapshot_exit(exit, expected.len()).unwrap(), expected);
    }
    let refusal = Refusal::Exhausted {
        what: "owned snapshot scalar trailer",
        bound: usize::MAX,
    };
    let snapshot = snapshot_refusal(&refusal, MAX_MESSAGE).unwrap();
    let mut count = Writer::counting(snapshot.len() - 1);
    count.header(kind::REFUSED, 0);
    put_refusal(&mut count, &refusal).unwrap();
    assert_eq!((count.out.len(), count.out.capacity()), (0, 0));
    assert!(matches!(count.counted(), Err(Fault::Oversized { .. })));
}

/// AC-T7.4.1: exact write mode refuses before an append can allocate, including
/// a missing reserve and a scalar overrun; storage failures retain their typed mapping.
#[test]
fn snapshot_fixed_writer_never_grows_after_reservation() {
    let exit = Exit::Code(7);
    let expected = answer(0, &Answer::Exited(exit), MAX_MESSAGE).unwrap();
    let mut output = Writer::fixed(expected.len()).unwrap();
    let allocated = output.out.capacity();
    output.header(kind::EXITED, 0);
    put_exit(&mut output, exit);
    assert_eq!(output.out, expected);
    output.u64(0);
    assert_eq!(output.out.capacity(), allocated);
    assert_eq!(output.out.len(), expected.len());
    assert!(matches!(output.done(), Err(Fault::Oversized { .. })));

    // Drop the reserved buffer to challenge the independent capacity guard.
    let mut missing = Writer::fixed(expected.len()).unwrap();
    missing.out = Vec::new();
    missing.header(kind::EXITED, 0);
    put_exit(&mut missing, exit);
    assert_eq!((missing.out.len(), missing.out.capacity()), (0, 0));
    let fault = missing.done().unwrap_err();
    assert!(matches!(fault, Fault::Storage { .. }));
    assert!(matches!(Refusal::from(fault), Refusal::Exhausted { .. }));
    // Vec rejects this capacity arithmetically; it makes no huge allocation.
    assert!(matches!(
        Writer::fixed(usize::MAX),
        Err(Fault::Storage { .. })
    ));
}

fn physical_unavailable(backend: Class, cause: LifecycleCause) -> BackendUnavailable {
    BackendUnavailable::from_refusal(
        backend,
        Refusal::Backing {
            key: route_wire_key(),
            cause,
            why: None,
        },
    )
    .unwrap()
}

/// Joint plan step 14, (B) (ROOT, 2026-11-13: an Uncertain with no reason is a CLAUDE.md §3 gap): a backing's refusal
/// carries the refusing site's reason, appended after its cause, and its message says it. Do send a Backing refusal,
/// Uncertain, with a reason. Expect it read back as sent; the reason's option tag (1) after the cause, then its length
/// and bytes, so one with none (a byte 0) is a byte longer than before; and its message ending with the reason.
#[test]
fn w14_b_a_backing_refusal_carries_its_reason() {
    let why = "its scope's end wasn't seen after its stop";
    let reasoned = Answer::Refused(Refusal::Backing {
        key: route_wire_key(),
        cause: LifecycleCause::Uncertain,
        why: Some(why.to_owned()),
    });
    let bytes = answer(216, &reasoned, MAX_MESSAGE).unwrap();
    assert_eq!(
        bytes.len(),
        HEADER_BYTES + 1 + BACKING_KEY_BYTES + 1 + 1 + 4 + why.len()
    );
    let reason = bytes.len() - why.len() - 4 - 1;
    assert_eq!(bytes[reason], 1, "its reason's option tag");
    assert_eq!(&bytes[bytes.len() - why.len()..], why.as_bytes());
    assert_eq!(
        read_answer(&received(&bytes, 0), MAX_MESSAGE).unwrap().1,
        reasoned
    );
    let Answer::Refused(refusal) = &reasoned else {
        panic!("owned refusal fixture changed its variant");
    };
    assert!(
        refusal.to_string().ends_with(&format!("Uncertain: {why}")),
        "{refusal}"
    );
}

/// Joint plan step 14, (B): an unavailable VM backend's refusal keeps its reason in the physical report. Do report a
/// Firecracker backend unavailable, Uncertain, with a reason. Expect the report read back as sent, the observation's
/// bytes its class tag and its refusal's own.
#[test]
fn w14_b_an_unavailable_backends_reason_travels_in_the_physical_report() {
    let mut report = example_physical_report();
    report.unavailable = vec![
        BackendUnavailable::from_refusal(
            Class::Firecracker,
            Refusal::Backing {
                key: route_wire_key(),
                cause: LifecycleCause::Uncertain,
                why: Some("its pool record names another generation".to_owned()),
            },
        )
        .unwrap(),
    ];
    let expected = Answer::PhysicalReport(report);
    let bytes = answer(221, &expected, MAX_MESSAGE).unwrap();
    assert_eq!(
        read_answer(&received(&bytes, 0), MAX_MESSAGE).unwrap().1,
        expected
    );
    let Answer::PhysicalReport(report) = &expected else {
        panic!("owned physical report changed its variant");
    };
    let canonical = snapshot_refusal(&report.unavailable[0].cause, MAX_MESSAGE).unwrap();
    assert!(bytes.ends_with(&canonical[HEADER_BYTES..]));
    assert_eq!(answer(221, &expected, bytes.len()).unwrap(), bytes);
    assert!(matches!(
        answer(221, &expected, bytes.len() - 1),
        Err(Fault::Oversized { .. })
    ));
}

/// Qualified observer failures append to, rather than replace, actual native identity and load.
#[test]
fn physical_report_unavailable_roundtrips_qualified_backing_causes() {
    assert_eq!(PHYSICAL_UNAVAILABLE_BYTES, 52);
    for cause in [
        LifecycleCause::Pending,
        LifecycleCause::Busy,
        LifecycleCause::Uncertain,
    ] {
        let mut report = example_physical_report();
        let legacy = answer(
            220,
            &Answer::PhysicalReport(report.try_clone().unwrap()),
            MAX_MESSAGE,
        )
        .unwrap();
        report.unavailable = vec![
            physical_unavailable(Class::Firecracker, cause),
            physical_unavailable(Class::Shards, cause),
        ];
        let expected = Answer::PhysicalReport(report);
        let bytes = answer(220, &expected, MAX_MESSAGE).unwrap();
        assert_eq!(&bytes[..legacy.len()], legacy);
        assert_eq!(&bytes[legacy.len()..legacy.len() + 4], &2u32.to_le_bytes());
        assert_eq!(
            bytes.len(),
            legacy.len() + 4 + 2 * PHYSICAL_UNAVAILABLE_BYTES
        );
        let Answer::PhysicalReport(report) = &expected else {
            panic!("owned physical report changed its variant");
        };
        for (at, observation) in report.unavailable.iter().enumerate() {
            let start = legacy.len() + 4 + at * PHYSICAL_UNAVAILABLE_BYTES;
            assert_eq!(bytes[start], class_tag(observation.backend));
            let canonical = snapshot_refusal(&observation.cause, MAX_MESSAGE).unwrap();
            assert_eq!(
                &bytes[start + 1..start + PHYSICAL_UNAVAILABLE_BYTES],
                &canonical[HEADER_BYTES..]
            );
        }
        assert_eq!(
            read_answer(&received(&bytes, 0), MAX_MESSAGE).unwrap().1,
            expected
        );
        assert_eq!(answer(220, &expected, bytes.len()).unwrap(), bytes);
        assert!(matches!(
            answer(220, &expected, bytes.len() - 1),
            Err(Fault::Oversized { .. })
        ));
        // Whole-body preflight includes the new count and every scalar trailer.
        let mut short = Writer::new(bytes.len() - HEADER_BYTES - 1);
        let initial_capacity = short.out.capacity();
        assert!(matches!(
            put_physical_report(&mut short, report),
            Err(Fault::Oversized { .. })
        ));
        assert_eq!(
            (short.out.len(), short.out.capacity()),
            (0, initial_capacity)
        );
        assert!(matches!(
            read_answer(&received(&bytes, 1), MAX_MESSAGE),
            Err(Fault::Descriptors { .. })
        ));
    }
}

/// Counts and qualified fixed-size entries are checked before list reservation or refusal names.
#[test]
fn physical_report_unavailable_rejects_hostile_counts_and_entries() {
    let mut report = example_physical_report();
    let legacy = answer(
        221,
        &Answer::PhysicalReport(report.try_clone().unwrap()),
        MAX_MESSAGE,
    )
    .unwrap();
    report.unavailable = vec![
        physical_unavailable(Class::Firecracker, LifecycleCause::Pending),
        physical_unavailable(Class::Shards, LifecycleCause::Uncertain),
    ];
    let bytes = answer(221, &Answer::PhysicalReport(report), MAX_MESSAGE).unwrap();
    let entry = legacy.len() + size_of::<u32>();
    let refused = |bytes: &[u8]| {
        let fault = read_answer(&received(bytes, 0), MAX_MESSAGE).unwrap_err();
        route_wire_assert_redacted(&fault);
        assert!(!format!("{fault:?}").contains("private-unavailable"));
        fault
    };
    for count in [3u32, u32::MAX] {
        let mut raw = legacy.clone();
        raw.extend_from_slice(&count.to_le_bytes());
        assert!(matches!(
            refused(&raw),
            Fault::Value {
                what: "physical report.unavailable",
                ..
            }
        ));
    }
    for count in [1usize, 2] {
        let mut raw = legacy.clone();
        raw.extend_from_slice(&u32::try_from(count).unwrap().to_le_bytes());
        raw.resize(entry + count * PHYSICAL_UNAVAILABLE_BYTES - 1, 0);
        assert!(matches!(refused(&raw), Fault::Truncated { need, .. }
            if need == entry + count * PHYSICAL_UNAVAILABLE_BYTES));
    }
    for tag in [0, 1, 4, u8::MAX] {
        let mut raw = bytes.clone();
        raw[entry] = tag;
        assert!(matches!(refused(&raw), Fault::Value { .. }));
    }
    // A well-formed variable refusal body would intern this name if the tag guard were absent.
    const PRIVATE_NAME: &str = "private-unavailable-name";
    assert!(!KEPT.lock().unwrap().contains(PRIVATE_NAME));
    for tag in (0..=9).chain([11, u8::MAX]) {
        let mut body = Writer::new(MAX_MESSAGE);
        body.u8(class_tag(Class::Firecracker));
        body.u8(tag);
        body.string(PRIVATE_NAME).unwrap();
        body.string("private-unavailable-reason").unwrap();
        let mut raw = legacy.clone();
        raw.extend_from_slice(&1u32.to_le_bytes());
        raw.extend_from_slice(&body.done().unwrap());
        assert!(matches!(
            refused(&raw),
            Fault::Value {
                what: "physical report.unavailable",
                ..
            }
        ));
    }
    assert!(!KEPT.lock().unwrap().contains(PRIVATE_NAME));
    // 3 is Retired (joint plan step 14), 4 NodeVmEnded (joint plan step 39, A-6); the next cause is unknown. The
    // cause is the byte before its reason's option tag, an entry's last (joint plan step 14, (B)).
    for cause in [5, u8::MAX] {
        let mut raw = bytes.clone();
        raw[entry + PHYSICAL_UNAVAILABLE_BYTES - 2] = cause;
        assert!(matches!(
            refused(&raw),
            Fault::Value {
                what: "backing.cause",
                ..
            }
        ));
    }
    // A reason's option tag that is neither 0 nor 1.
    for tag in [2, u8::MAX] {
        let mut raw = bytes.clone();
        raw[entry + PHYSICAL_UNAVAILABLE_BYTES - 1] = tag;
        assert!(matches!(refused(&raw), Fault::Value { what: "option", .. }));
    }
    let mut duplicate = bytes.clone();
    duplicate[entry + PHYSICAL_UNAVAILABLE_BYTES] = duplicate[entry];
    assert!(matches!(
        refused(&duplicate),
        Fault::Value {
            what: "physical report",
            ..
        }
    ));
    for observation in [
        BackendUnavailable {
            backend: Class::Oci,
            cause: Refusal::Backing {
                key: route_wire_key(),
                cause: LifecycleCause::Pending,
                why: None,
            },
        },
        BackendUnavailable {
            backend: Class::Shards,
            cause: Refusal::Broken("private-unavailable-reason".into()),
        },
    ] {
        let mut invalid = example_physical_report();
        invalid.unavailable.push(observation);
        let mut output = Writer::new(MAX_MESSAGE);
        assert!(matches!(
            put_physical_report(&mut output, &invalid),
            Err(Fault::Value {
                what: "physical report",
                ..
            })
        ));
        assert!(output.out.is_empty());
    }
}

/// Missing and explicit-empty tails retain legacy meaning; partial/new trailing data is refused.
#[test]
fn physical_report_unavailable_preserves_legacy_bytes_and_message_bounds() {
    let mut report = PhysicalReport {
        authority: "native".into(),
        devices: Vec::new(),
        unavailable: Vec::new(),
    };
    let legacy = answer(
        222,
        &Answer::PhysicalReport(report.try_clone().unwrap()),
        MAX_MESSAGE,
    )
    .unwrap();
    assert_eq!(&legacy[12..14], &0x800eu16.to_le_bytes());
    assert_eq!(
        &legacy[HEADER_BYTES..],
        b"\x06\x00\x00\x00native\x00\x00\x00\x00"
    );
    let mut explicit_empty = legacy.clone();
    explicit_empty.extend_from_slice(&0u32.to_le_bytes());
    for raw in [&legacy, &explicit_empty] {
        assert_eq!(
            read_answer(&received(raw, 0), MAX_MESSAGE).unwrap().1,
            Answer::PhysicalReport(report.try_clone().unwrap())
        );
    }
    report
        .unavailable
        .push(physical_unavailable(Class::Shards, LifecycleCause::Busy));
    let bytes = answer(222, &Answer::PhysicalReport(report), MAX_MESSAGE).unwrap();
    // The exact old boundary is intentionally valid. Every nonempty proper prefix of the new
    // count/body is incomplete; it must not silently downgrade a diagnostic to legacy-empty.
    for end in legacy.len() + 1..bytes.len() {
        assert!(matches!(
            read_answer(&received(&bytes[..end], 0), MAX_MESSAGE),
            Err(Fault::Truncated { .. })
        ));
    }
    explicit_empty.push(0);
    assert!(matches!(
        read_answer(&received(&explicit_empty, 0), MAX_MESSAGE),
        Err(Fault::Leftover { bytes: 1, .. })
    ));
    let mut bad_authority = bytes;
    bad_authority[HEADER_BYTES + size_of::<u32>()] = 0;
    assert!(matches!(
        read_answer(&received(&bad_authority, 0), MAX_MESSAGE),
        Err(Fault::Value {
            what: "physical report",
            ..
        })
    ));
}

/// AC-T7.4.1 (Shards R04): a Connect refusal whose failure, phase, I/O kind or reply tag is unknown,
/// whose option byte is neither 0 nor 1, that is cut short, or that has a byte past its end: expect
/// each refused by type.
#[test]
fn connect_refusal_hostile_tags_cuts_and_trailing_bytes_are_refused() {
    let io = Refusal::Connect {
        port: 9,
        failure: ConnectFailure::Io {
            phase: ConnectPhase::Read,
            kind: IoKind::new(std::io::ErrorKind::ConnectionRefused),
            code: Some(111),
        },
    };
    let bytes = answer(5, &Answer::Refused(io), MAX_MESSAGE).unwrap();
    // Refusal tag, port, failure tag, phase, the kind's option and tag, the code's option and bytes.
    let body = HEADER_BYTES;
    assert_eq!(bytes.len(), body + 14);
    assert_eq!(&bytes[body..body + 10], &[11, 9, 0, 0, 0, 1, 2, 1, 2, 1]);
    let what = |bytes: &[u8]| match read_answer(&received(bytes, 0), MAX_MESSAGE) {
        Err(Fault::Value { what, .. }) => what,
        other => panic!("{other:?}"),
    };
    let past_named = u8::try_from(IoKind::NAMED.len()).unwrap();
    for (at, byte, expected) in [
        (5, 4, "connect failure"),
        (5, u8::MAX, "connect failure"),
        (6, 4, "connect phase"),
        (7, 2, "option"),
        (8, past_named, "io kind"),
        (8, u8::MAX, "io kind"),
        (9, 2, "option"),
    ] {
        let mut hostile = bytes.clone();
        hostile[body + at] = byte;
        assert_eq!(what(&hostile), expected, "body byte {at} set to {byte}");
    }
    let reply = Refusal::Connect {
        port: 9,
        failure: ConnectFailure::Reply(ConnectReply::NotOk),
    };
    let mut hostile = answer(5, &Answer::Refused(reply), MAX_MESSAGE).unwrap();
    *hostile.last_mut().unwrap() = 2;
    assert_eq!(what(&hostile), "connect reply");
    for cut in body..bytes.len() {
        assert!(
            matches!(
                read_answer(&received(&bytes[..cut], 0), MAX_MESSAGE),
                Err(Fault::Truncated { .. })
            ),
            "cut at {cut}"
        );
    }
    let mut long = bytes;
    long.push(0);
    assert_eq!(
        read_answer(&received(&long, 0), MAX_MESSAGE).unwrap_err(),
        Fault::Leftover {
            bytes: 1,
            kind: kind::REFUSED,
        }
    );
}

/// AC-T7.4.1 (CLAUDE.md §2): write every Connect refusal, and a Backing one beside them, into a
/// writer one byte short of its body: expect each refused as oversized before a byte of it is
/// written or any room taken, as a Backing refusal's is; and into a writer of exactly its body:
/// expect all of it there, in no more room than that.
#[test]
fn connect_refusal_reserves_its_whole_body_before_writing() {
    let mut refusals = connect_refusals();
    refusals.push(Refusal::Backing {
        key: route_wire_key(),
        cause: LifecycleCause::Busy,
        why: None,
    });
    refusals.push(Refusal::Backing {
        key: route_wire_key(),
        cause: LifecycleCause::Uncertain,
        why: Some("its scope's end wasn't seen after its stop".to_owned()),
    });
    for refusal in &refusals {
        let mut sized = Writer::new(MAX_MESSAGE);
        put_refusal(&mut sized, refusal).unwrap();
        let body = sized.out.len();
        let mut short = Writer::new(body - 1);
        let room = short.out.capacity();
        let refused = put_refusal(&mut short, refusal);
        assert!(
            matches!(refused, Err(Fault::Oversized { .. })),
            "{refusal:?} in {} bytes: {refused:?}",
            body - 1
        );
        assert_eq!(
            (short.out.len(), short.out.capacity()),
            (0, room),
            "{refusal:?}"
        );
        let mut exact = Writer::new(body);
        put_refusal(&mut exact, refusal).unwrap();
        assert_eq!(
            (exact.out.len(), exact.out.capacity()),
            (body, body),
            "{refusal:?}"
        );
    }
}

/// AC-T7.4.1 (joint plan step 12, Shards R04): an `Awaits` refusal is tag 13, after every refusal
/// before it, then the capability's place in `Upstream::ALL`. Do: answer each capability refused, alone
/// and as an `Uncertain` refusal's cause. Expect: two body bytes, [13, place], each read back as
/// written. Do: a capability tag past the last, a cut and a trailing byte. Expect: refused as
/// "upstream", Truncated and Leftover. Do: write it into a writer one byte short. Expect: refused
/// before a byte is written, as a Backing refusal is.
#[test]
fn an_awaits_refusal_is_tagged_after_every_refusal_and_its_capability_by_place() {
    let body = HEADER_BYTES;
    for (place, needs) in Upstream::ALL.into_iter().enumerate() {
        let refusal = Refusal::Awaits { needs };
        let bytes = answer(5, &Answer::Refused(refusal.clone()), MAX_MESSAGE).unwrap();
        assert_eq!(&bytes[body..], &[13, u8::try_from(place).unwrap()]);
        assert_eq!(
            read_answer(&received(&bytes, 0), MAX_MESSAGE).unwrap(),
            (
                Header {
                    kind: kind::REFUSED,
                    id: 5
                },
                Answer::Refused(refusal.clone())
            )
        );
        let uncertain = Refusal::Uncertain {
            id: id("awaits"),
            refusal: Box::new(refusal.clone()),
            cleanup: Box::new(refusal.clone()),
        };
        let bytes = answer(6, &Answer::Refused(uncertain.clone()), MAX_MESSAGE).unwrap();
        assert_eq!(
            read_answer(&received(&bytes, 0), MAX_MESSAGE).unwrap().1,
            Answer::Refused(uncertain)
        );
        let mut short = Writer::new(1);
        assert!(matches!(
            put_refusal(&mut short, &refusal),
            Err(Fault::Oversized { .. })
        ));
        assert!(short.out.is_empty());
    }
    let bytes = answer(
        5,
        &Answer::Refused(Refusal::Awaits {
            needs: Upstream::MemberExec,
        }),
        MAX_MESSAGE,
    )
    .unwrap();
    for tag in [u8::try_from(Upstream::ALL.len()).unwrap(), u8::MAX] {
        let mut hostile = bytes.clone();
        hostile[body + 1] = tag;
        assert!(
            matches!(
                read_answer(&received(&hostile, 0), MAX_MESSAGE),
                Err(Fault::Value {
                    what: "upstream",
                    ..
                })
            ),
            "capability tag {tag}"
        );
    }
    assert!(matches!(
        read_answer(&received(&bytes[..body + 1], 0), MAX_MESSAGE),
        Err(Fault::Truncated { .. })
    ));
    let mut long = bytes;
    long.push(0);
    assert_eq!(
        read_answer(&received(&long, 0), MAX_MESSAGE).unwrap_err(),
        Fault::Leftover {
            bytes: 1,
            kind: kind::REFUSED,
        }
    );
}

/// AC-T7.4.1 (Shards R04): take each kind std's own errno mapping gives that Hex has no tag for (on
/// Linux with Rust 1.99.1, `Uncategorized`, `InputOutputError`, `TooManyOpenFiles`, `FilesystemLoop`
/// and `InProgress`) as a CONNECT's I/O cause: expect it unnamed and apart from `Other`, read back
/// whole with its OS code exact, its bytes apart from the same cause named `Other`; and expect each
/// named kind to be its own tag.
#[test]
fn an_untagged_io_kind_reads_back_unnamed_and_apart_from_other_with_its_code() {
    for (tag, kind) in IoKind::NAMED.iter().enumerate() {
        assert_eq!(IoKind::NAMED.iter().position(|k| k == kind), Some(tag));
    }
    let untagged: Vec<(i32, std::io::ErrorKind)> = (1..=200)
        .map(|code| (code, std::io::Error::from_raw_os_error(code).kind()))
        .filter(|(_, kind)| !IoKind::NAMED.contains(kind))
        .collect();
    assert!(
        !untagged.is_empty(),
        "no errno here maps to an untagged kind"
    );
    let other = IoKind::new(std::io::ErrorKind::Other);
    let connect = |kind, code| {
        Answer::Refused(Refusal::Connect {
            port: 3,
            failure: ConnectFailure::Io {
                phase: ConnectPhase::Dial,
                kind,
                code: Some(code),
            },
        })
    };
    for (code, kind) in untagged {
        let unnamed = IoKind::new(kind);
        assert_eq!(
            (unnamed, unnamed.named()),
            (IoKind::UNNAMED, None),
            "{kind:?}"
        );
        assert_ne!(unnamed, other);
        let sent = connect(unnamed, code);
        let bytes = answer(4, &sent, MAX_MESSAGE).unwrap();
        assert_eq!(
            read_answer(&received(&bytes, 0), MAX_MESSAGE).unwrap().1,
            sent
        );
        assert_ne!(
            bytes,
            answer(4, &connect(other, code), MAX_MESSAGE).unwrap()
        );
    }
}

/// AC-T7.4.1 (joint plan step 12): a Shards image VM's spec is class tag 5, after every class before
/// it, then its pinned image and its command's option. Do: create one running its members alone and
/// one with a command. Expect: each read back as written, tag 5 before the image's length. Do: its
/// command's option byte 2, and the message cut inside the image. Expect: refused as "option" and
/// Truncated.
#[test]
fn a_shards_image_spec_is_class_tag_5_then_its_image_and_command() {
    let spec = |command: Option<Vec<String>>| BoxSpec {
        id: id("image-wire"),
        bounds: Bounds::default(),
        class: ClassSpec::ShardsImage(Box::new(ShardsImage {
            command,
            ..ShardsImage::new(hex_api::sim::SIM_IMAGE)
        })),
        owner: None,
        devices: Vec::new(),
        allocation: Some(hex_api::sim::sim_vm_allocation().unwrap()),
        route: None,
        band: 0,
    };
    let image = hex_api::sim::SIM_IMAGE.as_bytes();
    let length = u32::try_from(image.len()).unwrap().to_le_bytes();
    for command in [None, Some(vec!["sleep".to_owned(), "600".to_owned()])] {
        let create = Request::Call(Call::Create(Box::new(spec(command))));
        let bytes = request(91, &create, MAX_MESSAGE).unwrap();
        assert_eq!(
            read_request(&received(&bytes, 0), MAX_MESSAGE).unwrap().1,
            create
        );
        let at = bytes.windows(image.len()).position(|w| w == image).unwrap();
        assert_eq!(&bytes[at - 4..at], &length);
        assert_eq!(bytes[at - 5], 5);
        let mut hostile = bytes.clone();
        hostile[at + image.len()] = 2;
        assert!(matches!(
            read_request(&received(&hostile, 0), MAX_MESSAGE).map_err(|bad| bad.fault),
            Err(Fault::Value { what: "option", .. })
        ));
        assert!(matches!(
            read_request(&received(&bytes[..at + 1], 0), MAX_MESSAGE).map_err(|bad| bad.fault),
            Err(Fault::Truncated { .. })
        ));
    }
}

/// AC-T7.4.1 (joint plan steps 12 and 14): a usage answer's guest time follows its eight counters as
/// an option: tag 0 and a count of nanoseconds by vCPU, or tag 1 and what its backend awaits. Do: answer
/// each. Expect: read back as written, its option, tag and count where said. Do: a guest-time tag 2, and
/// more vCPUs than a VM's index space, written and read. Expect: each refused as "guest time".
#[test]
fn guest_time_follows_the_usage_counters_by_vcpu_or_what_it_awaits() {
    let counters = HEADER_BYTES + size_of::<[u64; 8]>();
    let usage = |guest| {
        Answer::Usage(Usage {
            pids: 1,
            guest,
            ..Usage::default()
        })
    };
    let per_vcpu = usage(Some(GuestTime::PerVcpuNs(vec![5, 7])));
    let bytes = answer(4, &per_vcpu, MAX_MESSAGE).unwrap();
    assert_eq!(&bytes[counters..counters + 6], &[1, 0, 2, 0, 0, 0]);
    // Then the microVM's start's option byte (D253): none here.
    assert_eq!(bytes.len(), counters + 6 + 2 * size_of::<u64>() + 1);
    assert_eq!(
        read_answer(&received(&bytes, 0), MAX_MESSAGE).unwrap().1,
        per_vcpu
    );
    let awaits = usage(Some(GuestTime::Awaits(Upstream::GuestCpuTimePerVcpu)));
    let bytes = answer(4, &awaits, MAX_MESSAGE).unwrap();
    assert_eq!(&bytes[counters..], &[1, 1, 0, 0]);
    assert_eq!(
        read_answer(&received(&bytes, 0), MAX_MESSAGE).unwrap().1,
        awaits
    );
    let mut hostile = bytes;
    hostile[counters + 1] = 2;
    assert!(matches!(
        read_answer(&received(&hostile, 0), MAX_MESSAGE),
        Err(Fault::Value {
            what: "guest time",
            ..
        })
    ));
    let past = usage(Some(GuestTime::PerVcpuNs(vec![
        0;
        GuestTime::MAX_VCPUS + 1
    ])));
    assert!(matches!(
        answer(4, &past, MAX_MESSAGE),
        Err(Fault::Value {
            what: "guest time",
            ..
        })
    ));
    let mut read_past = answer(
        4,
        &usage(Some(GuestTime::PerVcpuNs(Vec::new()))),
        MAX_MESSAGE,
    )
    .unwrap();
    let count = u32::try_from(GuestTime::MAX_VCPUS + 1).unwrap();
    // Its count and the start's option byte after it, written again with a count past the index space.
    read_past.truncate(read_past.len() - size_of::<u32>() - 1);
    read_past.extend_from_slice(&count.to_le_bytes());
    read_past.extend(std::iter::repeat_n(
        0,
        (GuestTime::MAX_VCPUS + 1) * size_of::<u64>() + 1,
    ));
    assert!(matches!(
        read_answer(&received(&read_past, 0), MAX_MESSAGE),
        Err(Fault::Value {
            what: "guest time",
            ..
        })
    ));
}

fn learned_resources(cpu_milli: u64, gib: u64) -> CapacityResources {
    CapacityResources {
        cpu_milli,
        mem_bytes: gib << 30,
        disk_bytes: 0,
    }
}

fn learned_holding(id: &str, cpu_milli: u64, gib: u64) -> LearnedHolding {
    LearnedHolding {
        id: LearnedHolder::Box(BoxId::new(id).unwrap()),
        holding: learned_resources(cpu_milli, gib),
    }
}

fn learned_end(id: &str, gib: u64) -> LearnedEnd {
    LearnedEnd {
        id: LearnedHolder::Box(BoxId::new(id).unwrap()),
        held: gib << 30,
        budget: (gib - 1) << 30,
        band: 0,
        fleet_held: 17 << 30,
        fleet_growth: 2 << 30,
        fleet_budget: 16 << 30,
        unmoved: Some(Upstream::LiveMove),
    }
}

fn learned_capacity() -> Capacity {
    let mut capacity = device_capacity(Vec::new());
    capacity.learned = Some(Learned {
        priors: vec![LearnedPrior {
            profile: [3; 32],
            ceiling: learned_resources(2_000, 4),
            booking: learned_resources(300, 1),
        }],
        used: LearnedUse {
            booked: learned_resources(4_000, 8),
            reservation: learned_resources(900, 3),
        },
        holdings: vec![
            learned_holding("hex-place-a-1-0-7", 600, 2),
            learned_holding("hex-place-a-1-0-8", 300, 1),
        ],
        ended: vec![learned_end("hex-place-a-1-0-6", 5)],
        ceilings: vec![LearnedCeiling {
            profile: [3; 32],
            envelope: learned_resources(2_000, 4),
            overhead: LearnedOverhead {
                cpu_milli: Some(120),
                mem_bytes: None,
                disk_bytes: Some(0),
            },
        }],
        rates: Vec::new(),
        window: None,
        unread: Vec::new(),
        awaits: Vec::new(),
        ranked: Vec::new(),
        unrecorded: Vec::new(),
        unacted: Vec::new(),
        reservations: None,
    });
    capacity
}

/// The review's A-10 (AC-T7.4.1; ROOT, 2026-11-14): what a node's learner takes off its fleet's budget for the capacity
/// view's holders crosses the wire last on a class's learned answer, each state told. Do encode a capacity answer telling
/// none served, kiln's reservations unheard, and a keeper's word heard with and without the node's configuration naming
/// it; then one whose state byte names no state. Expect each back exactly, and the last refused, naming the field.
#[test]
fn a10_kilns_reservations_cross_the_wire_on_a_learned_answer() {
    for told in [
        LearnedReservations::Unserved,
        LearnedReservations::Unheard,
        LearnedReservations::Heard {
            keeper: 3,
            reserved: learned_resources(1_500, 6),
            unconfigured: false,
        },
        LearnedReservations::Heard {
            keeper: u64::MAX,
            reserved: learned_resources(0, 0),
            unconfigured: true,
        },
    ] {
        let mut capacity = learned_capacity();
        if let Some(learned) = capacity.learned.as_mut() {
            learned.reservations = Some(told);
        }
        let offered = Answer::Capacity(Some(capacity));
        let bytes = answer(14, &offered, MAX_MESSAGE).unwrap();
        assert_eq!(
            read_answer(&received(&bytes, 0), MAX_MESSAGE).unwrap().1,
            offered
        );
    }
    // The state's byte: where an answer telling it unheard and one telling none served differ, and only there.
    let encoded = |told| {
        let mut capacity = learned_capacity();
        if let Some(learned) = capacity.learned.as_mut() {
            learned.reservations = Some(told);
        }
        answer(14, &Answer::Capacity(Some(capacity)), MAX_MESSAGE).unwrap()
    };
    let mut bytes = encoded(LearnedReservations::Unheard);
    let unserved = encoded(LearnedReservations::Unserved);
    let apart: Vec<usize> = bytes
        .iter()
        .zip(&unserved)
        .enumerate()
        .filter(|(_, (a, b))| a != b)
        .map(|(at, _)| at)
        .collect();
    assert_eq!(apart.len(), 1, "one state byte apart: {apart:?}");
    bytes[apart[0]] = 9;
    let refused = read_answer(&received(&bytes, 0), MAX_MESSAGE).unwrap_err();
    assert!(
        refused.to_string().contains("learned reservations"),
        "{refused}"
    );
}

/// Joint plan step 14 (AC-T7.4.1, C2, C5 and C8): what a node learned of a class's microVMs crosses the wire on its
/// capacity answer, before only its node VMs (joint plan step 39). Do: encode a capacity with its priors, learned use,
/// holdings and the VMs its last resort ended, and one with none; cut the second before its last byte; read and write
/// both as JSON, and read the first's JSON without its ends. Expect both back exactly, the second ending with its none
/// tag, its machine's time unread and no node VM's count, and refused once cut, as every truncation is, JSON without the
/// field reading as none, and JSON without the ends as none ended.
#[test]
fn w14_a_capacitys_learned_use_crosses_the_wire_last() {
    let learned = learned_capacity();
    let offered = Answer::Capacity(Some(learned.try_clone().unwrap()));
    let bytes = answer(14, &offered, MAX_MESSAGE).unwrap();
    assert_eq!(
        read_answer(&received(&bytes, 0), MAX_MESSAGE).unwrap().1,
        offered
    );
    let none = device_capacity(Vec::new());
    let mut cut = answer(
        14,
        &Answer::Capacity(Some(none.try_clone().unwrap())),
        MAX_MESSAGE,
    )
    .unwrap();
    assert_eq!(
        cut[cut.len() - 9..],
        [0; 9],
        "no learned use, its machine's time unread, then no node VM's count, no learner's why and its boxes' CPU time \
         unread, ends it"
    );
    cut.truncate(cut.len() - 1);
    assert!(matches!(
        read_answer(&received(&cut, 0), MAX_MESSAGE),
        Err(Fault::Truncated { .. })
    ));
    let older = serde_json::to_value(&none).unwrap();
    assert!(older.get("learned").is_none(), "skipped when none");
    assert_eq!(serde_json::from_value::<Capacity>(older).unwrap(), none);
    let mut json = serde_json::to_value(&learned).unwrap();
    assert_eq!(
        serde_json::from_value::<Capacity>(json.clone()).unwrap(),
        learned
    );
    json["learned"].as_object_mut().unwrap().remove("ended");
    let unended = serde_json::from_value::<Capacity>(json).unwrap();
    assert!(unended.learned.unwrap().ended.is_empty());
}

/// Joint plan step 39 (ROOT's ruling of 2026-11-13; goal 14's wire): a node VM the node runs for itself is a holder in
/// its learned capacity beside its boxes. Do: encode a capacity whose holdings and ends name a node VM beside a box, and
/// read it back. Expect it back exactly.
#[test]
fn a_node_vms_learned_holding_and_end_cross_the_wire() {
    let mut capacity = learned_capacity();
    let run = LearnedHolder::Node(hex_api::NodeRun::from_bytes([5; 16]));
    if let Some(learned) = capacity.learned.as_mut() {
        learned.holdings.push(LearnedHolding {
            id: run.clone(),
            holding: learned_resources(1_000, 2),
        });
        learned.ended.push(LearnedEnd {
            id: run,
            ..learned_end("unused", 3)
        });
    }
    let offered = Answer::Capacity(Some(capacity));
    let bytes = answer(14, &offered, MAX_MESSAGE).unwrap();
    assert_eq!(
        read_answer(&received(&bytes, 0), MAX_MESSAGE).unwrap().1,
        offered
    );
}

fn node_vm_receipt(run: u8, phase: NodeVmPhase) -> NodeVmReceipt {
    NodeVmReceipt {
        run: hex_api::NodeRun::from_bytes([run; 16]),
        booking: learned_resources(2_000, 4),
        phase,
        guest: learned_resources(1_500, 3),
    }
}

/// Joint plan step 39 (AC-T7.4.1; ROOT, 2026-11-13: a node VM's receipt reaches the scheduler on its node's capacity
/// answer, its record the one source): the node VMs a node runs for itself cross the wire on its capacity answer, last.
/// Do: encode a capacity naming one node VM in each phase and read it back; read its JSON without them; check one naming
/// a run twice; read one whose receipts' count says one more than a node's boxes, and one whose last phase is past the
/// phases. Expect the first back exactly, the JSON read as naming none, and the other three refused.
#[test]
fn w39_a_capacitys_node_vms_cross_the_wire_last() {
    let mut capacity = learned_capacity();
    capacity.node_vms = vec![
        node_vm_receipt(1, NodeVmPhase::Booting),
        node_vm_receipt(2, NodeVmPhase::Ready),
        node_vm_receipt(3, NodeVmPhase::Ended),
    ];
    let offered = Answer::Capacity(Some(capacity.try_clone().unwrap()));
    let bytes = answer(39, &offered, MAX_MESSAGE).unwrap();
    assert_eq!(
        read_answer(&received(&bytes, 0), MAX_MESSAGE).unwrap().1,
        offered
    );
    let mut json = serde_json::to_value(&capacity).unwrap();
    json.as_object_mut().unwrap().remove("node_vms");
    assert!(
        serde_json::from_value::<Capacity>(json)
            .unwrap()
            .node_vms
            .is_empty()
    );
    let mut twice = capacity.try_clone().unwrap();
    twice.node_vms[1].run = twice.node_vms[0].run;
    assert!(twice.check().is_err());
    // The receipts' count, then each receipt: its run, its booking, its phase and its guest's room (V2), the last of
    // which comes just before the learner's why, none here (A-3), and its boxes' CPU time, unread here (A-8), which
    // ends it.
    let receipt = 16 + 3 * size_of::<u64>() + 1 + 3 * size_of::<u64>();
    let at = bytes.len() - (size_of::<u32>() + 3 * receipt + 1 + 1);
    let mut past = bytes.clone();
    let count = u32::try_from(MAX_LEARNED_HOLDINGS + 1).unwrap();
    past[at..at + size_of::<u32>()].copy_from_slice(&count.to_le_bytes());
    assert!(matches!(
        read_answer(&received(&past, 0), MAX_MESSAGE),
        Err(Fault::Value { .. })
    ));
    let mut unknown = bytes;
    // The last receipt's phase: before its guest's room, the learner's why and the boxes' CPU time.
    let phase = unknown.len() - 3 - 3 * size_of::<u64>();
    unknown[phase] = 3;
    assert!(matches!(
        read_answer(&received(&unknown, 0), MAX_MESSAGE),
        Err(Fault::Value { .. })
    ));
}

/// Joint plan step 14 (D255; ROOT, 2026-11-13): what holds each running managed VM's CPU to its rate crosses the wire
/// on its class's capacity answer, after the ceilings, where its node tells it. Do: encode a capacity whose learned
/// rates name a box its platform's cap holds, with why, one its governor holds under its job, and a node VM its
/// governor holds; read it back, and write and read it as JSON. Expect it back exactly each way.
#[test]
fn w14_d255_what_holds_each_managed_vms_rate_crosses_the_wire() {
    let mut capacity = learned_capacity();
    if let Some(learned) = capacity.learned.as_mut() {
        learned.rates = vec![
            hex_api::LearnedRate {
                id: LearnedHolder::Box(BoxId::new("hex-place-a-1-0-7").unwrap()),
                held: hex_api::RateHeld::Platform("its VM process takes no rate".into()),
            },
            hex_api::LearnedRate {
                id: LearnedHolder::Box(BoxId::new("hex-place-a-1-0-8").unwrap()),
                held: hex_api::RateHeld::GovernorUnderJob,
            },
            hex_api::LearnedRate {
                id: LearnedHolder::Node(hex_api::NodeRun::from_bytes([5; 16])),
                held: hex_api::RateHeld::Governor,
            },
        ];
    }
    let offered = Answer::Capacity(Some(capacity.try_clone().unwrap()));
    let bytes = answer(14, &offered, MAX_MESSAGE).unwrap();
    assert_eq!(
        read_answer(&received(&bytes, 0), MAX_MESSAGE).unwrap().1,
        offered
    );
    let json = serde_json::to_value(&capacity).unwrap();
    assert_eq!(serde_json::from_value::<Capacity>(json).unwrap(), capacity);
}

/// Joint plan step 14 (C1; ROOT, 2026-11-13): the learner's window crosses the wire on a class's capacity answer, after
/// the rates, and the VM whose platform cap's cycle sets it where one does. Do encode a capacity whose learned window is
/// 2 s, set by a node VM's cap, then one whose window no VM's cap sets; read each back, and write and read each as JSON.
/// Expect each back exactly each way.
#[test]
fn w14_the_learners_window_and_the_vm_that_sets_it_cross_the_wire() {
    for slowed_by in [
        Some(LearnedHolder::Node(hex_api::NodeRun::from_bytes([5; 16]))),
        None,
    ] {
        let mut capacity = learned_capacity();
        if let Some(learned) = capacity.learned.as_mut() {
            learned.window = Some(hex_api::LearnedWindow {
                window_ns: 2_000_000_000,
                slowed_by,
            });
        }
        let offered = Answer::Capacity(Some(capacity.try_clone().unwrap()));
        let bytes = answer(14, &offered, MAX_MESSAGE).unwrap();
        assert_eq!(
            read_answer(&received(&bytes, 0), MAX_MESSAGE).unwrap().1,
            offered
        );
        let json = serde_json::to_value(&capacity).unwrap();
        assert_eq!(serde_json::from_value::<Capacity>(json).unwrap(), capacity);
    }
}

/// Joint plan step 14 (D260; ROOT, 2026-11-13): a VM whose VMM's own CPU isn't read apart from its vCPUs', told why, and
/// one whose VMM's own took its whole share cross the wire on its class's capacity answer, appended after D255's. Do
/// encode a capacity whose learned rates name each, beside one its governor holds; read it back, and write and read it
/// as JSON. Expect it back exactly each way.
#[test]
fn w14_d260_a_vms_own_unheld_and_its_own_taking_its_share_cross_the_wire() {
    let mut capacity = learned_capacity();
    if let Some(learned) = capacity.learned.as_mut() {
        learned.rates = vec![
            hex_api::LearnedRate {
                id: LearnedHolder::Box(BoxId::new("hex-place-a-1-0-7").unwrap()),
                held: hex_api::RateHeld::OwnTakesShare {
                    own: 4_200,
                    share: 4_000,
                },
            },
            hex_api::LearnedRate {
                id: LearnedHolder::Box(BoxId::new("hex-place-a-1-0-8").unwrap()),
                held: hex_api::RateHeld::GovernorOwnUnheld("the microVM has not started".into()),
            },
            hex_api::LearnedRate {
                id: LearnedHolder::Node(hex_api::NodeRun::from_bytes([5; 16])),
                held: hex_api::RateHeld::Governor,
            },
        ];
    }
    let offered = Answer::Capacity(Some(capacity.try_clone().unwrap()));
    let bytes = answer(14, &offered, MAX_MESSAGE).unwrap();
    assert_eq!(
        read_answer(&received(&bytes, 0), MAX_MESSAGE).unwrap().1,
        offered
    );
    let json = serde_json::to_value(&capacity).unwrap();
    assert_eq!(serde_json::from_value::<Capacity>(json).unwrap(), capacity);
}

/// Joint plan step 14 (AC-T7.4.1): a capacity's learned use is held to its bounds on both sides. Do: encode one with
/// a prior more than a class has profiles, a prior booked past its ceiling, and a box's holding twice; then read one
/// whose holdings' count says one more than a node's boxes. Expect each refused, the last before any holding is read.
#[test]
fn w14_a_capacitys_learned_use_is_held_to_its_bounds() {
    let prior = learned_capacity().learned.unwrap().priors[0];
    let mut too_many = learned_capacity();
    if let Some(learned) = too_many.learned.as_mut() {
        learned.priors = (0..=MAX_LEARNED_PRIORS)
            .map(|n| LearnedPrior {
                profile: [u8::try_from(n % 256).unwrap(); 32],
                ceiling: learned_resources(2_000 + u64::try_from(n).unwrap(), 4),
                ..prior
            })
            .collect();
    }
    assert!(answer(14, &Answer::Capacity(Some(too_many)), MAX_MESSAGE).is_err());
    let mut past = learned_capacity();
    if let Some(learned) = past.learned.as_mut() {
        learned.priors[0].booking = learned_resources(2_001, 1);
    }
    assert!(answer(14, &Answer::Capacity(Some(past)), MAX_MESSAGE).is_err());
    let mut twice = learned_capacity();
    if let Some(learned) = twice.learned.as_mut() {
        learned.holdings[1].id = learned.holdings[0].id.clone();
    }
    assert!(answer(14, &Answer::Capacity(Some(twice)), MAX_MESSAGE).is_err());
    let mut one = learned_capacity();
    if let Some(learned) = one.learned.as_mut() {
        learned.holdings = vec![learned_holding("x", 1, 1)];
        learned.ended = Vec::new();
        learned.ceilings = Vec::new();
    }
    let mut bytes = answer(14, &Answer::Capacity(Some(one)), MAX_MESSAGE).unwrap();
    // The holdings' count, then the one holding (its holder's tag, its id's length, the id and its resources), then no
    // end's count, no ceiling's and no rate's, no window (C1), no unread VM's count, no await's and no ranked VM's
    // (place 4), no unrecorded box's (the review's dropped records), no unacted VM's (A-7), its machine's time unread,
    // no node VM's count, no learner's why (A-3) and its boxes' CPU time unread (A-8).
    let at = bytes.len() - (11 * size_of::<u32>() + 1 + 1 + 3 * size_of::<u64>() + 1 + 2 + 1 + 1);
    let count = u32::try_from(MAX_LEARNED_HOLDINGS + 1).unwrap();
    bytes[at..at + size_of::<u32>()].copy_from_slice(&count.to_le_bytes());
    assert!(matches!(
        read_answer(&received(&bytes, 0), MAX_MESSAGE),
        Err(Fault::Value { .. })
    ));
}

/// Joint plan step 14 (AC-T7.4.1, C1): a capacity's learned ceilings are held to its bounds and cross the wire whole.
/// Do: round-trip one whose ceiling has a cold dimension; check one with more ceilings than a class has profiles, then
/// one naming a profile and envelope twice. Expect the first read back as it was, each cold dimension still none; the
/// other two refused.
#[test]
fn w14_a_capacitys_ceilings_are_held_to_its_bounds() {
    let bytes = answer(14, &Answer::Capacity(Some(learned_capacity())), MAX_MESSAGE).unwrap();
    assert_eq!(
        read_answer(&received(&bytes, 0), MAX_MESSAGE).unwrap().1,
        Answer::Capacity(Some(learned_capacity()))
    );
    let ceiling = learned_capacity().learned.unwrap().ceilings[0];
    let mut too_many = learned_capacity();
    if let Some(learned) = too_many.learned.as_mut() {
        learned.ceilings = (0..=MAX_LEARNED_PRIORS)
            .map(|n| LearnedCeiling {
                profile: [u8::try_from(n % 256).unwrap(); 32],
                envelope: learned_resources(2_000 + u64::try_from(n).unwrap(), 4),
                ..ceiling
            })
            .collect();
    }
    assert!(too_many.learned.as_ref().unwrap().check().is_err());
    let mut twice = learned_capacity();
    if let Some(learned) = twice.learned.as_mut() {
        learned.ceilings.push(ceiling);
    }
    assert!(twice.learned.as_ref().unwrap().check().is_err());
}

/// Joint plan step 14 (AC-T7.4.1, C8): a node's holdings are kept to what one message holds, in their order. Do: fit a
/// capacity whose holdings fit, then one with more 128-byte box ids than a message has room for. Expect the first
/// unchanged; the second cut to a prefix of its holdings, encoded within the message, with room for none more.
#[test]
fn w14_a_capacitys_holdings_are_fitted_to_one_message_in_their_order() {
    let mut fits = learned_capacity();
    fit_learned(&mut fits).unwrap();
    assert_eq!(fits, learned_capacity());
    let each = size_of::<u32>() + BoxId::MAX_LEN + 3 * size_of::<u64>();
    let holdings: Vec<LearnedHolding> = (0..MAX_MESSAGE / each + 100)
        .map(|n| learned_holding(&format!("{n:0>128}"), 1, 1))
        .collect();
    let mut full = learned_capacity();
    if let Some(learned) = full.learned.as_mut() {
        learned.holdings = holdings.clone();
    }
    fit_learned(&mut full).unwrap();
    let kept = full.learned.as_ref().unwrap().holdings.len();
    assert!(
        kept > 0 && kept < holdings.len(),
        "{kept} of {}",
        holdings.len()
    );
    assert_eq!(full.learned.as_ref().unwrap().holdings, holdings[..kept]);
    let bytes = answer(
        14,
        &Answer::Capacity(Some(full.try_clone().unwrap())),
        MAX_MESSAGE,
    )
    .unwrap();
    assert!(bytes.len() <= MAX_MESSAGE);
    let mut more = full;
    if let Some(learned) = more.learned.as_mut() {
        learned.holdings.push(holdings[kept].clone());
    }
    assert!(answer(14, &Answer::Capacity(Some(more)), MAX_MESSAGE).is_err());
}

/// Joint plan step 14 (AC-T7.4.1, C5): a node's ends are held to its bounds and fitted after its holdings, latest
/// first. Do: check one with more ends than a node has boxes; read one whose ends' count says one more; fit one whose
/// holdings leave room for some of its many 128-byte-id ends. Expect the first two refused, the second before any end
/// is read; the third's holdings kept whole and its ends cut to a prefix, encoded within the message.
#[test]
fn w14_a_capacitys_ends_are_held_to_its_bounds_and_fitted_after_its_holdings() {
    let mut too_many = learned_capacity();
    if let Some(learned) = too_many.learned.as_mut() {
        learned.ended = vec![learned_end("x", 5); MAX_LEARNED_HOLDINGS + 1];
    }
    assert!(too_many.learned.as_ref().unwrap().check().is_err());
    let mut one = learned_capacity();
    if let Some(learned) = one.learned.as_mut() {
        learned.ended = vec![learned_end("x", 5)];
        learned.ceilings = Vec::new();
    }
    let mut bytes = answer(14, &Answer::Capacity(Some(one)), MAX_MESSAGE).unwrap();
    // The ends' count, then the one end: its holder's tag, its id's length, the id, five u64s, its band and its unmoved
    // capability's option and tag; then no ceiling's count and no rate's, no window (C1), no unread VM's count, no
    // await's and no ranked VM's (place 4), no unrecorded box's (the review's dropped records), no unacted VM's (A-7),
    // its machine's time unread, no node VM's count, no learner's why (A-3) and its boxes' CPU time unread (A-8): five
    // counts and lengths besides place 4's three, the unrecorded boxes' one and the unacted VMs' one.
    let counts = 5 * size_of::<u32>() + 3 * size_of::<u32>() + 2 * size_of::<u32>();
    let at = bytes.len() - (counts + 1 + 1 + 5 * size_of::<u64>() + 1 + 2 + 1 + 2 + 1 + 1);
    let count = u32::try_from(MAX_LEARNED_HOLDINGS + 1).unwrap();
    bytes[at..at + size_of::<u32>()].copy_from_slice(&count.to_le_bytes());
    assert!(matches!(
        read_answer(&received(&bytes, 0), MAX_MESSAGE),
        Err(Fault::Value { .. })
    ));
    let each = size_of::<u32>() + BoxId::MAX_LEN + 5 * size_of::<u64>() + 1 + 2;
    let ends: Vec<LearnedEnd> = (0..MAX_MESSAGE / each + 100)
        .map(|n| learned_end(&format!("{n:0>128}"), 5))
        .collect();
    let mut full = learned_capacity();
    if let Some(learned) = full.learned.as_mut() {
        learned.ended = ends.clone();
    }
    fit_learned(&mut full).unwrap();
    let learned = full.learned.as_ref().unwrap();
    assert_eq!(
        learned.holdings,
        learned_capacity().learned.unwrap().holdings
    );
    let kept = learned.ended.len();
    assert!(kept > 0 && kept < ends.len(), "{kept} of {}", ends.len());
    assert_eq!(learned.ended, ends[..kept]);
    let bytes = answer(14, &Answer::Capacity(Some(full)), MAX_MESSAGE).unwrap();
    assert!(bytes.len() <= MAX_MESSAGE);
}

/// What ends an info without grants on the wire (joint plan step 15): its empty grant list's u32 count, then its
/// empty list of published ports'.
const NO_GRANTS: usize = 2 * size_of::<u32>();
/// What follows an info's grants where its node's last resort didn't end it (joint plan step 14): its end's none tag.
const NO_END: usize = 1;
/// What ends an info whose run didn't fail (joint plan step 12): its words' none tag, after the last resort's.
const NO_WORDS: usize = 1;
/// What ends an info granting no credential (joint plan step 33): its empty credential list's u32 count, after its
/// words' tag.
const NO_CREDENTIALS: usize = size_of::<u32>();
/// What ends an info whose run told no agent-control endpoint (joint plan step 15, N6): its none tag, after its
/// credentials.
const NO_CONTROL: usize = 1;
/// What ends an info whose run told no agent-control endpoint's waker and wake (Shards' D252 amended, image channel
/// v30): its none tag, after the endpoint's.
const NO_WAKE: usize = 1;

/// Joint plan step 14 (AC-T7.4.1, C5): why a node's last resort ended a VM crosses the wire after its info's grants. Do:
/// encode an info with an end and one with none, cut the second before its end's none tag, and read and write both as
/// JSON. Expect both back exactly, the second's end a none tag and refused once cut, as every truncation is, and JSON
/// without the field reading as none.
#[test]
fn w14_an_infos_last_resort_crosses_the_wire_after_its_grants() {
    let ended = BoxInfo {
        last_resort: Some(learned_end("b-1", 5)),
        ..info()
    };
    let bytes = answer(3, &Answer::Info(ended.clone()), MAX_MESSAGE).unwrap();
    assert_eq!(
        read_answer(&received(&bytes, 0), MAX_MESSAGE).unwrap().1,
        Answer::Info(ended.clone())
    );
    let plain = info();
    let mut cut = answer(3, &Answer::Info(plain.clone()), MAX_MESSAGE).unwrap();
    assert_eq!(
        cut[cut.len() - NO_WAKE - NO_CONTROL - NO_CREDENTIALS - NO_WORDS - NO_END],
        0,
        "no end's tag"
    );
    cut.truncate(cut.len() - NO_WAKE - NO_CONTROL - NO_CREDENTIALS - NO_WORDS - NO_END);
    assert!(matches!(
        read_answer(&received(&cut, 0), MAX_MESSAGE),
        Err(Fault::Truncated { .. })
    ));
    let json = serde_json::to_value(&ended).unwrap();
    assert_eq!(serde_json::from_value::<BoxInfo>(json).unwrap(), ended);
    let older = serde_json::to_value(&plain).unwrap();
    assert!(older.get("last_resort").is_none(), "skipped when none");
    assert_eq!(serde_json::from_value::<BoxInfo>(older).unwrap(), plain);
}

/// Joint plan step 12 (AC-T7.4.1; HEX-IMG-3): why a box's run ended where it failed, in its runtime's words, crosses the
/// wire last on its info. Do: encode an info with words and one with none, cut the second before its last byte, and read
/// and write both as JSON. Expect both back exactly, the second ending with its none tag and refused once cut, as every
/// truncation is, words that aren't UTF-8 refused by name, and JSON without the field reading as none.
#[test]
fn w12_an_infos_run_words_cross_the_wire_last() {
    let words =
        "its command never started (status 128): exec: \"/bin/sh\": operation not permitted";
    let failed = BoxInfo {
        class: Class::Shards,
        exit: Some(Exit::Unknown),
        ended: Some(words.to_owned()),
        ..info()
    };
    let bytes = answer(5, &Answer::Info(failed.clone()), MAX_MESSAGE).unwrap();
    assert_eq!(
        read_answer(&received(&bytes, 0), MAX_MESSAGE).unwrap().1,
        Answer::Info(failed.clone())
    );
    assert!(
        bytes[..bytes.len() - NO_CREDENTIALS - NO_CONTROL - NO_WAKE].ends_with(words.as_bytes())
    );
    let mut garbled = bytes.clone();
    let last = garbled.len() - NO_CREDENTIALS - NO_CONTROL - NO_WAKE - 1;
    garbled[last] = 0xff;
    assert!(matches!(
        read_answer(&received(&garbled, 0), MAX_MESSAGE),
        Err(Fault::Value { what: "ended", .. })
    ));
    let plain = info();
    let mut cut = answer(5, &Answer::Info(plain.clone()), MAX_MESSAGE).unwrap();
    assert_eq!(
        cut[cut.len() - NO_CREDENTIALS - NO_CONTROL - NO_WAKE - 1],
        0,
        "no words' tag ends it, but for its credentials and its agent-control endpoint"
    );
    cut.truncate(cut.len() - NO_CREDENTIALS - NO_CONTROL - NO_WAKE - NO_WORDS);
    assert!(matches!(
        read_answer(&received(&cut, 0), MAX_MESSAGE),
        Err(Fault::Truncated { .. })
    ));
    let json = serde_json::to_value(&failed).unwrap();
    assert_eq!(serde_json::from_value::<BoxInfo>(json).unwrap(), failed);
    let older = serde_json::to_value(&plain).unwrap();
    assert!(older.get("ended").is_none(), "skipped when none");
    assert_eq!(serde_json::from_value::<BoxInfo>(older).unwrap(), plain);
}

/// Joint plan step 12 (D253; AC-T7.4.1): a usage answer says how a Shards image VM's microVM started, after its guest
/// time, as an option: tag 1 and its place in `StartedFrom` (Booted 0, Saved 1, Restored 2, Replaced 3, Warm 4); a box
/// with no Shards image run says none, a byte 0. Do answer each, then one whose tag is 5, then each as JSON. Expect each
/// read back as written, its tag its last byte; none a 0 last; tag 5 refused as "started from"; the JSON named in
/// kebab-case and left out where none.
#[test]
fn w12_a_usages_microvm_start_crosses_the_wire_last() {
    let usage = |started_from| {
        Answer::Usage(Usage {
            pids: 1,
            started_from,
            ..Usage::default()
        })
    };
    for (tag, from) in [
        (0u8, StartedFrom::Booted),
        (1, StartedFrom::Saved),
        (2, StartedFrom::Restored),
        (3, StartedFrom::Replaced),
        (4, StartedFrom::Warm),
    ] {
        let said = usage(Some(from));
        let bytes = answer(4, &said, MAX_MESSAGE).unwrap();
        assert_eq!(&bytes[bytes.len() - 2..], &[1, tag], "{from:?}");
        assert_eq!(
            read_answer(&received(&bytes, 0), MAX_MESSAGE).unwrap().1,
            said
        );
    }
    let none = answer(4, &usage(None), MAX_MESSAGE).unwrap();
    assert_eq!(&none[none.len() - 2..], &[0, 0], "no guest time, no start");
    assert_eq!(
        read_answer(&received(&none, 0), MAX_MESSAGE).unwrap().1,
        usage(None)
    );
    let mut hostile = answer(4, &usage(Some(StartedFrom::Warm)), MAX_MESSAGE).unwrap();
    let last = hostile.len() - 1;
    hostile[last] = 5;
    assert!(matches!(
        read_answer(&received(&hostile, 0), MAX_MESSAGE),
        Err(Fault::Value {
            what: "started from",
            ..
        })
    ));
    let warm = Usage {
        started_from: Some(StartedFrom::Warm),
        ..Usage::default()
    };
    let json = serde_json::to_value(&warm).unwrap();
    assert_eq!(json["started_from"], "warm");
    assert_eq!(serde_json::from_value::<Usage>(json).unwrap(), warm);
    let older = serde_json::to_value(Usage::default()).unwrap();
    assert!(older.get("started_from").is_none(), "skipped when none");
    assert_eq!(
        serde_json::from_value::<Usage>(older).unwrap(),
        Usage::default()
    );
}

/// Joint plan step 14 (AC-T7.4.1), C5: a box's priority band reaches its node. Do send a create of a spec in band 3
/// through the node's wire, then the same cut by its last byte, and the spec as JSON, then a band-0 spec as JSON. Expect
/// the create back exactly, its band its last byte, the cut one refused as every truncation is, and the JSON back
/// exactly, a band-0 spec's naming no band (its record and commitment those of a spec before the band).
#[test]
fn w14_a_specs_band_crosses_the_wire_last() {
    let banded = BoxSpec {
        band: 3,
        ..oci_spec()
    };
    let call = Request::Call(Call::Create(Box::new(banded.clone())));
    let bytes = request(4, &call, MAX_MESSAGE).unwrap();
    assert_eq!(
        read_request(&received(&bytes, 0), MAX_MESSAGE).unwrap().1,
        call
    );
    assert_eq!(bytes.last(), Some(&3), "the band ends it");
    let cut = &bytes[..bytes.len() - 1];
    assert!(matches!(
        read_request(&received(cut, 0), MAX_MESSAGE).map_err(|bad| bad.fault),
        Err(Fault::Truncated { .. })
    ));
    let json = serde_json::to_value(&banded).unwrap();
    assert_eq!(serde_json::from_value::<BoxSpec>(json).unwrap(), banded);
    let plain = oci_spec();
    let older = serde_json::to_value(&plain).unwrap();
    assert!(older.get("band").is_none(), "none named at band 0");
    assert_eq!(serde_json::from_value::<BoxSpec>(older).unwrap(), plain);
}

fn harness() -> Member {
    Member {
        kind: MemberKind::Harness,
        name: "codex".to_owned(),
    }
}

/// One grant of each way, as kiln would give a harness: a provider by name, kiln on the VM's host, and one port
/// published on the host's loopback.
fn each_way() -> Vec<Grant> {
    vec![
        Grant::Egress {
            member: harness(),
            to: Destination::Name("api.openai.com".to_owned()),
            ports: Ports {
                proto: Proto::Tcp,
                first: 443,
                last: 443,
            },
        },
        Grant::Host {
            member: harness(),
            ports: Ports {
                proto: Proto::Udp,
                first: 7070,
                last: 7071,
            },
        },
        Grant::Publish {
            member: harness(),
            proto: Proto::Tcp,
            port: 1455,
            host: "127.0.0.1".parse().unwrap(),
            host_port: None,
        },
    ]
}

/// The bytes `list` encodes to with the check skipped, as a peer that skipped it would send them.
fn unchecked(list: &[Grant]) -> Vec<u8> {
    let mut w = Writer::new(MAX_MESSAGE);
    w.list(list, put_grant).unwrap();
    w.done().unwrap()
}

/// Joint plan step 15, HEX-SOCK-2: do write one grant of each way and read it back; expect the same grants, the
/// bytes exactly the fixed tags' (way, member kind, IANA protocol number, address family), and nothing left over.
#[test]
fn grants_survive_the_wire_with_fixed_tags() {
    let list = each_way();
    let mut w = Writer::new(MAX_MESSAGE);
    put_grants(&mut w, &list).unwrap();
    let bytes = w.done().unwrap();
    let text = |s: &str| {
        let mut out = u32::try_from(s.len()).unwrap().to_le_bytes().to_vec();
        out.extend_from_slice(s.as_bytes());
        out
    };
    let mut expected = 3u32.to_le_bytes().to_vec();
    expected.extend([0, 1]);
    expected.extend(text("codex"));
    expected.push(0);
    expected.extend(text("api.openai.com"));
    expected.push(6);
    expected.extend(443u16.to_le_bytes());
    expected.extend(443u16.to_le_bytes());
    expected.extend([1, 1]);
    expected.extend(text("codex"));
    expected.push(17);
    expected.extend(7070u16.to_le_bytes());
    expected.extend(7071u16.to_le_bytes());
    expected.extend([2, 1]);
    expected.extend(text("codex"));
    expected.push(6);
    expected.extend(1455u16.to_le_bytes());
    expected.extend([4, 127, 0, 0, 1, 0]);
    assert_eq!(bytes, expected);
    let mut r = Reader::new(&bytes);
    assert_eq!(grants(&mut r).unwrap(), list);
    assert_eq!(r.at(), bytes.len());
}

/// Joint plan step 15, AC-T7.7.2: do write grants the check refuses (a loopback destination, one flow named twice),
/// then read the same lists as a peer that skipped the check would send them; expect each refused both ways, typed
/// as invalid grants, so no list the check refuses crosses the wire.
#[test]
fn a_grant_list_the_check_refuses_is_neither_written_nor_read() {
    let mut loopback = each_way();
    loopback.push(Grant::Egress {
        member: harness(),
        to: Destination::Address("127.0.0.1".parse().unwrap()),
        ports: Ports {
            proto: Proto::Tcp,
            first: 7070,
            last: 7070,
        },
    });
    let mut twice = each_way();
    twice.push(twice[0].clone());
    for list in [loopback, twice] {
        let mut w = Writer::new(MAX_MESSAGE);
        assert!(
            matches!(
                put_grants(&mut w, &list),
                Err(Fault::Value { what: "grants", .. })
            ),
            "{list:?} was written"
        );
        let bytes = unchecked(&list);
        assert!(
            matches!(
                grants(&mut Reader::new(&bytes)),
                Err(Fault::Value { what: "grants", .. })
            ),
            "{list:?} was read"
        );
    }
}

/// Joint plan step 15, HEX-SOCK-1: do read a grant whose way, destination, member kind, protocol or address family
/// is a tag no grant has; expect each refused by the field's name, never read as another.
#[test]
fn unknown_grant_tags_are_refused_by_name() {
    let good = unchecked(&each_way()[..1]);
    // The single egress grant's tags: its way at 4 (after the count), its member's kind at 5, its destination's at
    // 15 (after the name's length and "codex") and its protocol at 34 (after the name "api.openai.com").
    for (at, tag, field) in [
        (4, 3, "grant"),
        (5, 2, "grant member"),
        (15, 2, "grant destination"),
        (34, 7, "grant protocol"),
    ] {
        let mut bytes = good.clone();
        bytes[at] = tag;
        assert!(
            matches!(grants(&mut Reader::new(&bytes)), Err(Fault::Value { what, .. }) if what == field),
            "tag {tag} at {at}: {:?}",
            grants(&mut Reader::new(&bytes))
        );
    }
    let publish = unchecked(&each_way()[2..]);
    // The publish's address family follows its way, member, protocol and port: at 4 + 1 + 1 + 4 + 5 + 1 + 2.
    let mut bytes = publish.clone();
    bytes[18] = 5;
    assert!(
        matches!(
            grants(&mut Reader::new(&bytes)),
            Err(Fault::Value {
                what: "grant address",
                ..
            })
        ),
        "{:?}",
        grants(&mut Reader::new(&bytes))
    );
    for cut in 0..publish.len() {
        assert!(
            grants(&mut Reader::new(&publish[..cut])).is_err(),
            "cut at {cut}"
        );
    }
}

/// Joint plan step 17, AC-T7.4.1: do create a Shards image VM whose spec names the members its caller drives, one
/// with variables, a working directory and run-time arguments (D256, kiln's step 17; an empty one among them) and one
/// with none of them; expect it read back whole, its members after its grants, each member's arguments after its
/// working directory.
#[test]
fn a_shards_image_vm_carries_the_members_its_caller_drives_on_the_wire() {
    let members = vec![
        hex_api::ImageMember {
            member: Member {
                kind: MemberKind::Harness,
                name: "claude".to_owned(),
            },
            env: vec![("LANG".to_owned(), "C.UTF-8".to_owned())],
            workdir: Some("/home/me/project".to_owned()),
            args: vec![
                "--resume".to_owned(),
                "s-1".to_owned(),
                String::new(),
                "--model".to_owned(),
                "claude-opus-4".to_owned(),
            ],
            mcp: false,
        },
        hex_api::ImageMember::new(Member {
            kind: MemberKind::Agent,
            name: "helper".to_owned(),
        }),
    ];
    let create = Request::Call(Call::Create(Box::new(BoxSpec {
        id: id("image-members"),
        bounds: Bounds::default(),
        class: ClassSpec::ShardsImage(Box::new(ShardsImage {
            members,
            ..ShardsImage::new(hex_api::sim::SIM_IMAGE)
        })),
        owner: None,
        devices: Vec::new(),
        allocation: Some(hex_api::sim::sim_vm_allocation().unwrap()),
        route: None,
        band: 0,
    })));
    let bytes = request(93, &create, MAX_MESSAGE).unwrap();
    assert_eq!(
        read_request(&received(&bytes, 0), MAX_MESSAGE).unwrap().1,
        create
    );
}

/// Joint plan step 12, Shards' D254 (AC-T7.4.1): do create a Shards image VM driving a harness that asks an MCP bridge
/// of its own, with run-time arguments, and an agent that asks none; then the same VM whose harness asks none. Expect
/// the first read back whole, and its bytes the second's but for one: each member's ask is one byte after its
/// run-time arguments.
#[test]
fn w12_a_members_mcp_bridge_crosses_the_wire_after_its_arguments() {
    let create = |mcp: bool| {
        Request::Call(Call::Create(Box::new(BoxSpec {
            id: id("image-mcp"),
            bounds: Bounds::default(),
            class: ClassSpec::ShardsImage(Box::new(ShardsImage {
                members: vec![
                    hex_api::ImageMember {
                        args: vec!["--model".to_owned(), "claude-opus-4".to_owned()],
                        mcp,
                        ..hex_api::ImageMember::new(Member {
                            kind: MemberKind::Harness,
                            name: "claude".to_owned(),
                        })
                    },
                    hex_api::ImageMember::new(Member {
                        kind: MemberKind::Agent,
                        name: "helper".to_owned(),
                    }),
                ],
                ..ShardsImage::new(hex_api::sim::SIM_IMAGE)
            })),
            owner: None,
            devices: Vec::new(),
            allocation: Some(hex_api::sim::sim_vm_allocation().unwrap()),
            route: None,
            band: 0,
        })))
    };
    let asked = create(true);
    let bytes = request(94, &asked, MAX_MESSAGE).unwrap();
    assert_eq!(
        read_request(&received(&bytes, 0), MAX_MESSAGE).unwrap().1,
        asked
    );
    let none = request(94, &create(false), MAX_MESSAGE).unwrap();
    assert_eq!(none.len(), bytes.len());
    assert_eq!(none.iter().zip(&bytes).filter(|(a, b)| a != b).count(), 1);
}

/// Joint plan step 12 for kiln's step 18 (AC-T7.4.1): do create a Shards image VM sharing two host directories, one
/// read-write and one read-only, beside a member its caller drives. Expect it read back whole, its shares after its
/// members, each its host directory, where its guest sees it and whether read-only.
#[test]
fn w12_a_shards_image_vms_shares_cross_the_wire_after_its_members() {
    let create = Request::Call(Call::Create(Box::new(BoxSpec {
        id: id("image-shares"),
        bounds: Bounds::default(),
        class: ClassSpec::ShardsImage(Box::new(ShardsImage {
            members: vec![hex_api::ImageMember::new(Member {
                kind: MemberKind::Harness,
                name: "claude".to_owned(),
            })],
            shares: vec![
                Mount {
                    source: PathBuf::from("/var/lib/kiln/homes/ada/.claude"),
                    destination: "/home/agent/.claude".to_owned(),
                    read_only: false,
                },
                Mount {
                    source: PathBuf::from("/srv/projects/hex"),
                    destination: "/work/hex".to_owned(),
                    read_only: true,
                },
            ],
            ..ShardsImage::new(hex_api::sim::SIM_IMAGE)
        })),
        owner: None,
        devices: Vec::new(),
        allocation: Some(hex_api::sim::sim_vm_allocation().unwrap()),
        route: None,
        band: 0,
    })));
    let bytes = request(94, &create, MAX_MESSAGE).unwrap();
    assert_eq!(
        read_request(&received(&bytes, 0), MAX_MESSAGE).unwrap().1,
        create
    );
}

/// Joint plan step 15 (every grant visible in Hex's record of the VM): do create a Shards image VM granted a provider
/// by name, kiln on its host and a publish; expect it read back whole, its grants written after its command. Do
/// write one whose grants the check refuses (a loopback destination), and read it as a peer that skipped the check
/// would send it; expect both refused as invalid grants.
#[test]
fn a_shards_image_vm_carries_its_grants_on_the_wire() {
    let spec = |grants: Vec<Grant>| BoxSpec {
        id: id("image-grants"),
        bounds: Bounds::default(),
        class: ClassSpec::ShardsImage(Box::new(ShardsImage {
            grants,
            ..ShardsImage::new(hex_api::sim::SIM_IMAGE)
        })),
        owner: None,
        devices: Vec::new(),
        allocation: Some(hex_api::sim::sim_vm_allocation().unwrap()),
        route: None,
        band: 0,
    };
    let create = Request::Call(Call::Create(Box::new(spec(each_way()))));
    let bytes = request(92, &create, MAX_MESSAGE).unwrap();
    assert_eq!(
        read_request(&received(&bytes, 0), MAX_MESSAGE).unwrap().1,
        create
    );
    let mut listed = Writer::new(MAX_MESSAGE);
    put_grants(&mut listed, &each_way()).unwrap();
    let listed = listed.done().unwrap();
    let image = hex_api::sim::SIM_IMAGE.as_bytes();
    let named = bytes.windows(image.len()).position(|w| w == image).unwrap();
    // After the image, the command's option (none), then the grants.
    let at = named + image.len() + 1;
    assert_eq!(bytes[at - 1], 0);
    assert_eq!(&bytes[at..at + listed.len()], listed.as_slice());
    let mut loopback = each_way();
    loopback.push(Grant::Egress {
        member: harness(),
        to: Destination::Address("127.0.0.1".parse().unwrap()),
        ports: Ports {
            proto: Proto::Tcp,
            first: 7070,
            last: 7070,
        },
    });
    let refused = Request::Call(Call::Create(Box::new(spec(loopback.clone()))));
    assert!(matches!(
        request(93, &refused, MAX_MESSAGE),
        Err(Fault::Value { what: "grants", .. })
    ));
    let mut spliced = bytes[..at].to_vec();
    spliced.extend(unchecked(&loopback));
    spliced.extend_from_slice(&bytes[at + listed.len()..]);
    assert!(matches!(
        read_request(&received(&spliced, 0), MAX_MESSAGE).map_err(|bad| bad.fault),
        Err(Fault::Value { what: "grants", .. })
    ));
}

/// Joint plan step 15 (N6): do answer an inspect of a running Shards image VM with its grants and its publish as
/// the host bound it, its receiver the member Shards named (at its own port) and, as an older run said it, the
/// image's declaration. Expect: each read back whole, the bound port's grant, socket and receiver last but for the
/// last resort's none tag (joint plan step 14): the named receiver its tag 1 and the member's port, the declared
/// one its tag 0. Do: answer one whose report names an egress grant, and read one whose receiver tag no receiver
/// has. Expect: refused as "published" written, and by the receiver's name read.
#[test]
fn an_inspect_carries_its_grants_and_their_ports_as_bound() {
    let published = Published {
        grant: 2,
        at: "127.0.0.1:49152".parse().unwrap(),
        receiver: Receiver::Named { port: 1455 },
    };
    let shown = Answer::Info(BoxInfo {
        class: Class::Shards,
        grants: each_way(),
        published: vec![published],
        ..info()
    });
    let bytes = answer(94, &shown, MAX_MESSAGE).unwrap();
    assert_eq!(
        read_answer(&received(&bytes, 0), MAX_MESSAGE).unwrap().1,
        shown
    );
    let mut tail = 1u32.to_le_bytes().to_vec();
    tail.extend(2u32.to_le_bytes());
    tail.extend([4, 127, 0, 0, 1]);
    tail.extend(49152u16.to_le_bytes());
    tail.push(1);
    tail.extend(1455u16.to_le_bytes());
    tail.extend([0; NO_END + NO_WORDS + NO_CREDENTIALS + NO_CONTROL + NO_WAKE]);
    assert!(bytes.ends_with(&tail));
    let declared = Answer::Info(BoxInfo {
        class: Class::Shards,
        grants: each_way(),
        published: vec![Published {
            receiver: Receiver::Declared,
            ..published
        }],
        ..info()
    });
    let older = answer(96, &declared, MAX_MESSAGE).unwrap();
    assert_eq!(
        read_answer(&received(&older, 0), MAX_MESSAGE).unwrap().1,
        declared
    );
    assert!(
        older.ends_with(
            &[
                &[0][..],
                &[0; NO_END + NO_WORDS + NO_CREDENTIALS + NO_CONTROL + NO_WAKE][..]
            ]
            .concat()
        )
    );
    let wrong = Answer::Info(BoxInfo {
        class: Class::Shards,
        grants: each_way(),
        published: vec![Published {
            grant: 0,
            ..published
        }],
        ..info()
    });
    assert!(matches!(
        answer(95, &wrong, MAX_MESSAGE),
        Err(Fault::Value {
            what: "published",
            ..
        })
    ));
    let mut hostile = bytes.clone();
    let receiver = hostile.len()
        - 1
        - size_of::<u16>()
        - NO_END
        - NO_WORDS
        - NO_CREDENTIALS
        - NO_CONTROL
        - NO_WAKE;
    hostile[receiver] = 2;
    assert!(matches!(
        read_answer(&received(&hostile, 0), MAX_MESSAGE),
        Err(Fault::Value {
            what: "published receiver",
            ..
        })
    ));
}

fn any_member() -> impl Strategy<Value = Member> {
    (any::<bool>(), "[a-z][a-z0-9._-]{0,6}").prop_map(|(agent, name)| Member {
        kind: if agent {
            MemberKind::Agent
        } else {
            MemberKind::Harness
        },
        name,
    })
}

fn any_ports() -> impl Strategy<Value = Ports> {
    (any::<bool>(), 1u16..=u16::MAX, 0u16..64).prop_map(|(udp, first, span)| Ports {
        proto: if udp { Proto::Udp } else { Proto::Tcp },
        first,
        last: first.saturating_add(span),
    })
}

fn any_grant() -> impl Strategy<Value = Grant> {
    let to = prop_oneof![
        "[a-z]([a-z0-9-]{0,6}[a-z0-9])?\\.[a-z]{2,6}".prop_map(Destination::Name),
        any::<[u8; 4]>().prop_map(|a| Destination::Address(IpAddr::from(a))),
        any::<[u8; 16]>().prop_map(|a| Destination::Address(IpAddr::from(a))),
    ];
    let host = prop_oneof![
        Just("127.0.0.1"),
        Just("::1"),
        Just("10.0.0.2"),
        Just("fd00::2")
    ];
    prop_oneof![
        (any_member(), to, any_ports()).prop_map(|(member, to, ports)| Grant::Egress {
            member,
            to,
            ports
        }),
        (any_member(), any_ports()).prop_map(|(member, ports)| Grant::Host { member, ports }),
        (
            any_member(),
            any::<bool>(),
            1u16..=u16::MAX,
            host,
            proptest::option::of(1u16..=u16::MAX)
        )
            .prop_map(|(member, udp, port, host, host_port)| Grant::Publish {
                member,
                proto: if udp { Proto::Udp } else { Proto::Tcp },
                port,
                host: host.parse().unwrap(),
                host_port,
            }),
    ]
    .prop_filter("a well-formed grant", |g| {
        check_grants(std::slice::from_ref(g)).is_ok()
    })
}

proptest! {
    /// Joint plan step 15, HEX-SOCK-2: any well-formed list of grants of every way reads back as written, whole.
    #[test]
    fn any_grants_read_back_as_written(list in proptest::collection::vec(any_grant(), 0..6).prop_filter("each flow named once", |l| check_grants(l).is_ok())) {
        let mut w = Writer::new(MAX_MESSAGE);
        put_grants(&mut w, &list).unwrap();
        let bytes = w.done().unwrap();
        let mut r = Reader::new(&bytes);
        prop_assert_eq!(grants(&mut r).unwrap(), list);
        prop_assert_eq!(r.at(), bytes.len());
    }

    /// Joint plan step 15, HEX-SOCK-1: any bytes are a grant list or a typed refusal, never a panic.
    #[test]
    fn any_bytes_are_grants_or_refused(bytes in proptest::collection::vec(any::<u8>(), 0..256)) {
        let _ = grants(&mut Reader::new(&bytes));
    }
}

/// A placeholder as kiln's broker mints one (Shards' D242): a visible marker, then 256 random bits in hex.
const OPENAI_PLACEHOLDER: &str =
    "kiln-placeholder-0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";

/// Joint plan step 33's credential grants: one toward the provider `each_way` grants its harness by name, by TCP.
fn openai_credential(toward: &str) -> Vec<CredentialGrant> {
    vec![CredentialGrant {
        member: harness(),
        reference: "openai/ada".to_owned(),
        toward: vec![toward.to_owned()],
        placeholder: OPENAI_PLACEHOLDER.to_owned(),
    }]
}

/// Credential grants as a peer that skipped the check would write them.
fn unchecked_credentials(list: &[CredentialGrant]) -> Vec<u8> {
    let mut w = Writer::new(MAX_MESSAGE);
    w.list(list, |w, grant| {
        put_member(w, &grant.member)?;
        w.string(&grant.reference)?;
        w.list(&grant.toward, |w, host| w.string(host))?;
        w.string(&grant.placeholder)
    })
    .unwrap();
    w.done().unwrap()
}

/// Joint plan step 33 (every credential grant in Hex's record of the VM; Shards' D242): do create a Shards image VM
/// granting its harness a credential toward its provider, beside a share. Expect it read back whole, its credentials
/// written right after the share, last of the image. Do write one whose credential its harness's egress doesn't reach,
/// and read it as a peer that skipped the check would send it. Expect both refused as invalid credentials.
#[test]
fn w33_a_shards_image_vms_credential_grants_cross_the_wire_after_its_shares() {
    let spec = |credentials: Vec<CredentialGrant>| BoxSpec {
        id: id("image-credentials"),
        bounds: Bounds::default(),
        class: ClassSpec::ShardsImage(Box::new(ShardsImage {
            grants: each_way(),
            shares: vec![Mount {
                source: PathBuf::from("/srv/projects/hex"),
                destination: "/work/hex".to_owned(),
                read_only: true,
            }],
            credentials,
            ..ShardsImage::new(hex_api::sim::SIM_IMAGE)
        })),
        owner: None,
        devices: Vec::new(),
        allocation: Some(hex_api::sim::sim_vm_allocation().unwrap()),
        route: None,
        band: 0,
    };
    let granted = openai_credential("api.openai.com");
    let create = Request::Call(Call::Create(Box::new(spec(granted.clone()))));
    let bytes = request(95, &create, MAX_MESSAGE).unwrap();
    assert_eq!(
        read_request(&received(&bytes, 0), MAX_MESSAGE).unwrap().1,
        create
    );
    let mut listed = Writer::new(MAX_MESSAGE);
    put_credentials(&mut listed, &granted, &each_way()).unwrap();
    let listed = listed.done().unwrap();
    let share = b"/work/hex";
    // After the share's guest path, its read-only flag, then the credentials.
    let at = bytes.windows(share.len()).position(|w| w == share).unwrap() + share.len() + 1;
    assert_eq!(bytes[at - 1], 1);
    assert_eq!(&bytes[at..at + listed.len()], listed.as_slice());
    let unreached = openai_credential("evil.example");
    let refused = Request::Call(Call::Create(Box::new(spec(unreached.clone()))));
    assert!(matches!(
        request(96, &refused, MAX_MESSAGE),
        Err(Fault::Value {
            what: "credentials",
            ..
        })
    ));
    let mut spliced = bytes[..at].to_vec();
    spliced.extend(unchecked_credentials(&unreached));
    spliced.extend_from_slice(&bytes[at + listed.len()..]);
    assert!(matches!(
        read_request(&received(&spliced, 0), MAX_MESSAGE).map_err(|bad| bad.fault),
        Err(Fault::Value {
            what: "credentials",
            ..
        })
    ));
}

/// Joint plan step 33 (C4): do answer an inspect of a Shards image VM with its grants and its credential grants. Expect
/// it read back whole, its credentials its last bytes but for its agent-control endpoint's none tag (joint plan step 15).
/// Do answer one whose credential its grants don't reach. Expect it refused as "credentials" written.
#[test]
fn w33_an_inspect_carries_its_credential_grants_last() {
    let credentials = openai_credential("api.openai.com");
    let shown = Answer::Info(BoxInfo {
        class: Class::Shards,
        grants: each_way(),
        credentials: credentials.clone(),
        ..info()
    });
    let bytes = answer(97, &shown, MAX_MESSAGE).unwrap();
    assert_eq!(
        read_answer(&received(&bytes, 0), MAX_MESSAGE).unwrap().1,
        shown
    );
    let mut listed = Writer::new(MAX_MESSAGE);
    put_credentials(&mut listed, &credentials, &each_way()).unwrap();
    assert!(bytes[..bytes.len() - NO_CONTROL - NO_WAKE].ends_with(&listed.done().unwrap()));
    let unreached = Answer::Info(BoxInfo {
        class: Class::Shards,
        grants: Vec::new(),
        credentials,
        ..info()
    });
    assert!(matches!(
        answer(98, &unreached, MAX_MESSAGE),
        Err(Fault::Value {
            what: "credentials",
            ..
        })
    ));
}

/// Joint plan step 15 (N6; AC-T7.7.2): do answer an inspect of a running Shards image VM with its run's agent-control
/// endpoint, at an IPv4 loopback address and at IPv6's, and one with none; read each back, and write each as JSON. Expect
/// each back exactly, the endpoint its info's last bytes but its waker's and wake's none tag (tag 1, its address as a
/// grant's is written, its port), none a 0 there and refused once cut, as every truncation is, and JSON without the
/// field reading as none. Do read one whose
/// endpoint's address family is none, and one whose tag is neither 0 nor 1; expect each refused by name.
#[test]
fn an_inspect_carries_its_runs_agent_control_endpoint_last() {
    for (at, address) in [
        ("127.0.0.1:43471", [&[4u8][..], &[127, 0, 0, 1]].concat()),
        (
            "[::1]:43471",
            [&[6u8][..], &std::net::Ipv6Addr::LOCALHOST.octets()].concat(),
        ),
    ] {
        let shown = BoxInfo {
            class: Class::Shards,
            control: Some(at.parse().unwrap()),
            control_wake: None,
            ..info()
        };
        let bytes = answer(99, &Answer::Info(shown.clone()), MAX_MESSAGE).unwrap();
        assert_eq!(
            read_answer(&received(&bytes, 0), MAX_MESSAGE).unwrap().1,
            Answer::Info(shown.clone()),
            "{at}"
        );
        let tail = [&[1u8][..], &address, &43471u16.to_le_bytes(), &[0u8]].concat();
        assert!(bytes.ends_with(&tail), "{at}");
        let json = serde_json::to_value(&shown).unwrap();
        assert_eq!(serde_json::from_value::<BoxInfo>(json).unwrap(), shown);
        let (tag, family) = (bytes.len() - tail.len(), bytes.len() - tail.len() + 1);
        for (at_byte, byte, what) in [(family, 5, "grant address"), (tag, 2, "option")] {
            let mut hostile = bytes.clone();
            hostile[at_byte] = byte;
            assert!(
                matches!(
                    read_answer(&received(&hostile, 0), MAX_MESSAGE),
                    Err(Fault::Value { what: said, .. }) if said == what
                ),
                "{at}: byte {byte} at {at_byte}"
            );
        }
    }
    let plain = info();
    let mut cut = answer(99, &Answer::Info(plain.clone()), MAX_MESSAGE).unwrap();
    assert_eq!(cut.last(), Some(&0), "no waker's and wake's tag ends it");
    cut.truncate(cut.len() - NO_WAKE);
    assert!(matches!(
        read_answer(&received(&cut, 0), MAX_MESSAGE),
        Err(Fault::Truncated { .. })
    ));
    let older = serde_json::to_value(&plain).unwrap();
    assert!(older.get("control").is_none(), "skipped when none");
    assert_eq!(serde_json::from_value::<BoxInfo>(older).unwrap(), plain);
}

/// Shards' D252 amended (image channel v30; AC-T7.7.2): do answer an inspect of a running Shards image VM with its run's
/// agent-control endpoint and that endpoint's waker and wake, at IPv4 loopback addresses and at IPv6's; read each back,
/// and write each as JSON. Expect each back exactly, the waker and the wake its info's last bytes (tag 1, then each
/// address as a grant's is written and its port), every socket the endpoint holds its `control_sockets`, and JSON
/// without the field reading as none. Do read one telling a waker and wake with no endpoint; expect it refused by name.
#[test]
fn v30_an_inspect_carries_its_endpoints_waker_and_wake_after_it() {
    let v6 = |port: u16| std::net::SocketAddr::from((std::net::Ipv6Addr::LOCALHOST, port));
    let v4 = |port: u16| std::net::SocketAddr::from(([127, 0, 0, 1], port));
    for (control, waker, wake) in [
        (v4(43471), v4(50001), v4(50002)),
        (v6(43471), v6(50001), v6(50002)),
    ] {
        let shown = BoxInfo {
            class: Class::Shards,
            control: Some(control),
            control_wake: Some(ControlWake { waker, wake }),
            ..info()
        };
        assert_eq!(shown.control_sockets(), Some([control, waker, wake]));
        let bytes = answer(99, &Answer::Info(shown.clone()), MAX_MESSAGE).unwrap();
        assert_eq!(
            read_answer(&received(&bytes, 0), MAX_MESSAGE).unwrap().1,
            Answer::Info(shown.clone()),
            "{control}"
        );
        let address = |at: std::net::SocketAddr| match at.ip() {
            std::net::IpAddr::V4(ip) => [&[4u8][..], &ip.octets()].concat(),
            std::net::IpAddr::V6(ip) => [&[6u8][..], &ip.octets()].concat(),
        };
        let tail = [
            &[1u8][..],
            &address(waker),
            &waker.port().to_le_bytes(),
            &address(wake),
            &wake.port().to_le_bytes(),
        ]
        .concat();
        assert!(bytes.ends_with(&tail), "{control}");
        let json = serde_json::to_value(&shown).unwrap();
        assert_eq!(serde_json::from_value::<BoxInfo>(json).unwrap(), shown);
    }
    let unbound = BoxInfo {
        class: Class::Shards,
        control: None,
        control_wake: Some(ControlWake {
            waker: v4(50001),
            wake: v4(50002),
        }),
        ..info()
    };
    assert_eq!(unbound.control_sockets(), None);
    let bytes = answer(99, &Answer::Info(unbound), MAX_MESSAGE).unwrap();
    assert!(
        matches!(
            read_answer(&received(&bytes, 0), MAX_MESSAGE),
            Err(Fault::Value {
                what: "control_wake",
                ..
            })
        ),
        "a waker and wake with no endpoint"
    );
    let older = serde_json::to_value(info()).unwrap();
    assert!(older.get("control_wake").is_none(), "skipped when none");
}

/// docs/design/credential-grants.md C7: a lease's value crosses the wire in its own message alone, which says it carries
/// one so its holder wipes it ([`carries_secret`]); what reads it holds the value in a `Value`, and neither side prints it.
#[test]
fn s33_a_leases_value_crosses_marked_and_never_printed() {
    let value = b"hex-s33-a-credential-value";
    let leased = Request::Leased {
        run: 3,
        answer: Ok(Lease {
            lease: "lease-3".to_owned(),
            value: Value::new(value.to_vec()),
            life: None,
        }),
    };
    let bytes = request(4, &leased, MAX_MESSAGE).unwrap();
    assert!(
        carries_secret(&bytes),
        "a leased message says it carries a value"
    );
    for (n, other, _) in requests() {
        if !matches!(other, Request::Leased { answer: Ok(_), .. }) {
            let other = request(n, &other, MAX_MESSAGE).unwrap();
            let kind = other.get(12..14).map(|k| u16::from_le_bytes([k[0], k[1]]));
            assert_eq!(
                carries_secret(&other),
                matches!(kind, Some(0x001b..=0x001d)),
                "only a leased, captured or refreshed message says it carries a value"
            );
        }
    }
    for (n, other) in answers() {
        assert_eq!(
            carries_secret(&answer(n, &other, MAX_MESSAGE).unwrap()),
            matches!(other, Answer::CaptureAsk { .. }),
            "only a capture's ask carries tokens among the node's messages"
        );
    }
    let (_, back) = read_request(&received(&bytes, 0), MAX_MESSAGE).unwrap();
    assert_eq!(back, leased);
    let printed = format!("{back:?} {leased:?}");
    assert!(!printed.contains("hex-s33"), "a value printed: {printed}");
    assert!(
        printed.contains(&format!("Value({} bytes)", value.len())),
        "{printed}"
    );
}

/// docs/design/credential-grants.md C7: an answer to a lease ask names a lease or a refusal of Shards' kinds, and a
/// lessor's answer one made or a refusal: any other byte there is refused by type, naming it.
#[test]
fn s33_a_lease_answer_of_no_defined_kind_is_refused() {
    let undefined = |kind: u16, body: &[u8]| {
        let mut w = Writer::new(MAX_MESSAGE);
        w.header(kind, 5);
        w.raw(body);
        let bytes = w.done().unwrap();
        read_request(&received(&bytes, 0), MAX_MESSAGE)
    };
    for (kind, body) in [
        (0x001b_u16, [1u8, 0, 0, 0, 0, 0, 0, 0, 2].as_slice()),
        (0x001b, [1, 0, 0, 0, 0, 0, 0, 0, 1, 5].as_slice()),
        (0x001a, [2].as_slice()),
    ] {
        match undefined(kind, body) {
            Err(Bad {
                id: 5,
                fault: Fault::Value { .. },
            }) => {}
            other => panic!("kind {kind:#06x} body {body:?}: {other:?}"),
        }
    }
}

/// Place 4 (ROOT, 2026-11-13: what Hex decides on its own says why): what a learner couldn't read and why, what each
/// guest's room awaits, and each node VM ranked above every box and why cross the wire on a class's capacity answer,
/// last, after its window, a box and a node VM alike. Do encode a learned capacity naming a box and a node VM unread, a box
/// awaiting `GuestRoom` and a node VM ranked, and read it back; then its JSON without the three. Expect it back exactly,
/// and the JSON read as none of them.
#[test]
fn w39_what_a_learner_couldnt_read_awaits_and_ranks_crosses_the_wire_last() {
    let run = LearnedHolder::Node(hex_api::NodeRun::from_bytes([5; 16]));
    let boxed = LearnedHolder::Box(BoxId::new("hex-place-a-1-0-9").unwrap());
    let mut capacity = learned_capacity();
    if let Some(learned) = capacity.learned.as_mut() {
        learned.unread = vec![
            hex_api::LearnedUnread {
                id: boxed.clone(),
                why: "its cgroup's use can't be read".to_owned(),
            },
            hex_api::LearnedUnread {
                id: run.clone(),
                why: "its run isn't running here".to_owned(),
            },
        ];
        learned.awaits = vec![hex_api::LearnedAwait {
            id: boxed,
            needs: Upstream::GuestRoom,
        }];
        learned.ranked = vec![hex_api::LearnedRank {
            run: hex_api::NodeRun::from_bytes([5; 16]),
            why: "box torn-1's record can't be read: its record is torn".to_owned(),
        }];
    }
    let offered = Answer::Capacity(Some(capacity.try_clone().unwrap()));
    let bytes = answer(39, &offered, MAX_MESSAGE).unwrap();
    assert_eq!(
        read_answer(&received(&bytes, 0), MAX_MESSAGE).unwrap().1,
        offered
    );
    let mut json = serde_json::to_value(&capacity).unwrap();
    for field in ["unread", "awaits", "ranked"] {
        json["learned"].as_object_mut().unwrap().remove(field);
    }
    let older = serde_json::from_value::<Capacity>(json)
        .unwrap()
        .learned
        .unwrap();
    assert!(older.unread.is_empty() && older.awaits.is_empty() && older.ranked.is_empty());
}

/// Place 4 (ROOT, 2026-11-13: neither exceeds its bound under the most holders): what a learner couldn't read, what each
/// guest's room awaits and each node VM it ranks are each held to a node's boxes, one for each running VM, and fitted to
/// one message after its holdings, ends and rates, in their order. Do check learned uses naming one past a node's boxes of each; then fit a
/// capacity naming a node's boxes of each, and encode it. Expect each refused, typed, naming what passed its bound; the
/// fitted capacity within one message, each list a prefix of what it named, the first cut, and refused with one more.
#[test]
fn w39_what_a_learner_couldnt_read_awaits_and_ranks_is_held_to_a_nodes_boxes() {
    let holder =
        |n: usize| LearnedHolder::Box(BoxId::new(&format!("hex-place-a-1-0-{n}")).unwrap());
    let unread: Vec<hex_api::LearnedUnread> = (0..=MAX_LEARNED_HOLDINGS)
        .map(|n| hex_api::LearnedUnread {
            id: holder(n),
            why: "its cgroup's use can't be read".to_owned(),
        })
        .collect();
    let awaits: Vec<hex_api::LearnedAwait> = (0..=MAX_LEARNED_HOLDINGS)
        .map(|n| hex_api::LearnedAwait {
            id: holder(n),
            needs: Upstream::GuestRoom,
        })
        .collect();
    let ranked: Vec<hex_api::LearnedRank> = (0..=MAX_LEARNED_HOLDINGS)
        .map(|n| hex_api::LearnedRank {
            run: hex_api::NodeRun::from_bytes(u128::try_from(n).unwrap().to_le_bytes()),
            why: "box torn-1's record can't be read: its record is torn".to_owned(),
        })
        .collect();
    let learned = || learned_capacity().learned.unwrap();
    for (what, past) in [
        (
            "learned unread VMs",
            Learned {
                unread: unread.clone(),
                ..learned()
            },
        ),
        (
            "learned awaits",
            Learned {
                awaits: awaits.clone(),
                ..learned()
            },
        ),
        (
            "learned ranked VMs",
            Learned {
                ranked: ranked.clone(),
                ..learned()
            },
        ),
    ] {
        assert!(
            matches!(past.check(), Err(Refusal::Exhausted { what: named, bound }) if named == what && bound == MAX_LEARNED_HOLDINGS),
            "{what}: {:?}",
            past.check()
        );
    }
    let mut capacity = learned_capacity();
    capacity.learned = Some(Learned {
        unread: unread[..MAX_LEARNED_HOLDINGS].to_vec(),
        awaits: awaits[..MAX_LEARNED_HOLDINGS].to_vec(),
        ranked: ranked[..MAX_LEARNED_HOLDINGS].to_vec(),
        ..learned()
    });
    fit_learned(&mut capacity).unwrap();
    let fitted = capacity.learned.as_ref().unwrap();
    assert!(fitted.check().is_ok(), "{:?}", fitted.check());
    assert!(
        !fitted.unread.is_empty() && fitted.unread.len() < MAX_LEARNED_HOLDINGS,
        "{} of a node's boxes",
        fitted.unread.len()
    );
    assert_eq!(fitted.unread, unread[..fitted.unread.len()]);
    assert_eq!(fitted.awaits, awaits[..fitted.awaits.len()]);
    assert_eq!(fitted.ranked, ranked[..fitted.ranked.len()]);
    let bytes = answer(
        39,
        &Answer::Capacity(Some(capacity.try_clone().unwrap())),
        MAX_MESSAGE,
    )
    .unwrap();
    assert!(bytes.len() <= MAX_MESSAGE);
    let mut more = capacity;
    if let Some(learned) = more.learned.as_mut() {
        let next = learned.unread.len();
        learned.unread.push(unread[next].clone());
    }
    assert!(answer(39, &Answer::Capacity(Some(more)), MAX_MESSAGE).is_err());
}

/// The review's dropped records (ROOT, 2026-11-14; AC-T7.4.1): each box whose record its node can't read, and why,
/// crosses the wire on a class's capacity answer, last, after the node VMs it ranks above every box. Do encode a learned
/// capacity naming two such boxes and a node VM ranked, and read it back; then its JSON without the boxes. Expect it back
/// exactly, and the JSON read as naming none.
#[test]
fn w14_dropped_the_boxes_whose_records_cant_be_read_cross_the_wire_last() {
    let why = "its record can't be read: EOF while parsing a value; counted at its machine";
    let mut capacity = learned_capacity();
    if let Some(learned) = capacity.learned.as_mut() {
        learned.ranked = vec![hex_api::LearnedRank {
            run: hex_api::NodeRun::from_bytes([5; 16]),
            why: "box torn-1's record can't be read: EOF while parsing a value".to_owned(),
        }];
        learned.unrecorded = ["torn-1", "torn-2"]
            .map(|id| hex_api::LearnedUnrecorded {
                id: LearnedHolder::Box(BoxId::new(id).unwrap()),
                why: why.to_owned(),
            })
            .to_vec();
    }
    let offered = Answer::Capacity(Some(capacity.try_clone().unwrap()));
    let bytes = answer(14, &offered, MAX_MESSAGE).unwrap();
    assert_eq!(
        read_answer(&received(&bytes, 0), MAX_MESSAGE).unwrap().1,
        offered
    );
    let mut json = serde_json::to_value(&capacity).unwrap();
    json["learned"]
        .as_object_mut()
        .unwrap()
        .remove("unrecorded");
    let older = serde_json::from_value::<Capacity>(json)
        .unwrap()
        .learned
        .unwrap();
    assert!(older.unrecorded.is_empty());
}

/// The review's dropped records (ROOT, 2026-11-14: as place 4's lists are, each box once): the boxes whose records a
/// node can't read are held to a node's boxes, each named once, and fitted to one message after its ranked node VMs. Do
/// check learned uses naming one past a node's boxes, and one box twice; then fit a capacity naming a node's boxes, and
/// encode it. Expect the first refused, typed, naming its bound, and the second naming the repeat; the fitted capacity
/// within one message, its list a prefix of what it named, the first cut, and refused with one more.
#[test]
fn w14_dropped_the_boxes_whose_records_cant_be_read_are_held_to_a_nodes_boxes_each_once() {
    let told = |n: usize| hex_api::LearnedUnrecorded {
        id: LearnedHolder::Box(BoxId::new(&format!("hex-place-a-1-0-{n}")).unwrap()),
        why: "its record can't be read: EOF while parsing a value; counted at its machine"
            .to_owned(),
    };
    let unrecorded: Vec<hex_api::LearnedUnrecorded> =
        (0..=MAX_LEARNED_HOLDINGS).map(told).collect();
    let learned = || learned_capacity().learned.unwrap();
    let past = Learned {
        unrecorded: unrecorded.clone(),
        ..learned()
    };
    assert!(
        matches!(past.check(), Err(Refusal::Exhausted { what, bound }) if what == "learned unrecorded boxes" && bound == MAX_LEARNED_HOLDINGS),
        "{:?}",
        past.check()
    );
    let twice = Learned {
        unrecorded: vec![told(1), told(2), told(1)],
        ..learned()
    };
    assert!(
        matches!(twice.check(), Err(Refusal::Invalid { what, .. }) if what == "learned unrecorded boxes"),
        "{:?}",
        twice.check()
    );
    let mut capacity = learned_capacity();
    capacity.learned = Some(Learned {
        unrecorded: unrecorded[..MAX_LEARNED_HOLDINGS].to_vec(),
        ..learned()
    });
    fit_learned(&mut capacity).unwrap();
    let fitted = capacity.learned.as_ref().unwrap();
    assert!(fitted.check().is_ok(), "{:?}", fitted.check());
    assert!(
        !fitted.unrecorded.is_empty() && fitted.unrecorded.len() < MAX_LEARNED_HOLDINGS,
        "{} of a node's boxes",
        fitted.unrecorded.len()
    );
    assert_eq!(fitted.unrecorded, unrecorded[..fitted.unrecorded.len()]);
    let bytes = answer(
        14,
        &Answer::Capacity(Some(capacity.try_clone().unwrap())),
        MAX_MESSAGE,
    )
    .unwrap();
    assert!(bytes.len() <= MAX_MESSAGE);
    let mut more = capacity;
    if let Some(learned) = more.learned.as_mut() {
        let next = learned.unrecorded.len();
        learned.unrecorded.push(unrecorded[next].clone());
    }
    assert!(answer(14, &Answer::Capacity(Some(more)), MAX_MESSAGE).is_err());
}

/// A-7 (the review's, 2026-11-14; ROOT's ruling; AC-T7.4.1): each VM its node's window couldn't cap or tell its room,
/// held at what was last set, and why, crosses the wire on its class's capacity answer, last, after the boxes whose
/// records can't be read. Do encode a learned capacity naming two such VMs and an unrecorded box, and read it back;
/// then its JSON without the VMs. Expect it back exactly, and the JSON read as naming none.
#[test]
fn a7_the_vms_a_window_couldnt_act_on_cross_the_wire_last() {
    let mut capacity = learned_capacity();
    if let Some(learned) = capacity.learned.as_mut() {
        learned.unrecorded = vec![hex_api::LearnedUnrecorded {
            id: LearnedHolder::Box(BoxId::new("torn-1").unwrap()),
            why: "its record can't be read: EOF while parsing a value; counted at its machine"
                .to_owned(),
        }];
        learned.unacted = ["vm-1", "vm-2"]
            .map(|id| hex_api::LearnedUnacted {
                id: LearnedHolder::Box(BoxId::new(id).unwrap()),
                why: "its cap couldn't be set: the rate was refused; held at its last, 8000 milli-CPU".to_owned(),
            })
            .to_vec();
    }
    let offered = Answer::Capacity(Some(capacity.try_clone().unwrap()));
    let bytes = answer(14, &offered, MAX_MESSAGE).unwrap();
    assert_eq!(
        read_answer(&received(&bytes, 0), MAX_MESSAGE).unwrap().1,
        offered
    );
    let mut json = serde_json::to_value(&capacity).unwrap();
    json["learned"].as_object_mut().unwrap().remove("unacted");
    let older = serde_json::from_value::<Capacity>(json)
        .unwrap()
        .learned
        .unwrap();
    assert!(older.unacted.is_empty());
}

/// A-7 (ROOT, 2026-11-14: as place 4's lists are, each VM once): the VMs a window couldn't act on are held to a node's
/// VMs, each named once, and fitted to one message after its unrecorded boxes. Do check learned uses naming one past a
/// node's VMs, and one VM twice; then fit a capacity naming a node's VMs, and encode it. Expect the first refused,
/// typed, naming its bound, and the second naming the repeat; the fitted capacity within one message, its list a prefix
/// of what it named, the first cut, and refused with one more.
#[test]
fn a7_the_vms_a_window_couldnt_act_on_are_held_to_a_nodes_vms_each_once() {
    let told = |n: usize| hex_api::LearnedUnacted {
        id: LearnedHolder::Box(BoxId::new(&format!("hex-place-a-1-0-{n}")).unwrap()),
        why: "its cap couldn't be set: the rate was refused; held at its last, 8000 milli-CPU"
            .to_owned(),
    };
    let unacted: Vec<hex_api::LearnedUnacted> = (0..=MAX_LEARNED_HOLDINGS).map(told).collect();
    let learned = || learned_capacity().learned.unwrap();
    let past = Learned {
        unacted: unacted.clone(),
        ..learned()
    };
    assert!(
        matches!(past.check(), Err(Refusal::Exhausted { what, bound }) if what == "learned unacted VMs" && bound == MAX_LEARNED_HOLDINGS),
        "{:?}",
        past.check()
    );
    let twice = Learned {
        unacted: vec![told(1), told(2), told(1)],
        ..learned()
    };
    assert!(
        matches!(twice.check(), Err(Refusal::Invalid { what, .. }) if what == "learned unacted VMs"),
        "{:?}",
        twice.check()
    );
    let mut capacity = learned_capacity();
    capacity.learned = Some(Learned {
        unacted: unacted[..MAX_LEARNED_HOLDINGS].to_vec(),
        ..learned()
    });
    fit_learned(&mut capacity).unwrap();
    let fitted = capacity.learned.as_ref().unwrap();
    assert!(fitted.check().is_ok(), "{:?}", fitted.check());
    assert!(
        !fitted.unacted.is_empty() && fitted.unacted.len() < MAX_LEARNED_HOLDINGS,
        "{} of a node's VMs",
        fitted.unacted.len()
    );
    assert_eq!(fitted.unacted, unacted[..fitted.unacted.len()]);
    let bytes = answer(
        14,
        &Answer::Capacity(Some(capacity.try_clone().unwrap())),
        MAX_MESSAGE,
    )
    .unwrap();
    assert!(bytes.len() <= MAX_MESSAGE);
    let mut more = capacity;
    if let Some(learned) = more.learned.as_mut() {
        let next = learned.unacted.len();
        learned.unacted.push(unacted[next].clone());
    }
    assert!(answer(14, &Answer::Capacity(Some(more)), MAX_MESSAGE).is_err());
}

/// A-3 (ROOT's audit, 2026-11-13: a failed window was told nowhere): why a node's learner learned nothing for a class
/// crosses the wire on its capacity answer, last. Do encode a capacity naming why, and one naming none, and read both
/// back; then read the first's JSON without the field. Expect both back exactly, and the JSON read as naming none.
#[test]
fn w14_a3_why_a_learner_learned_nothing_crosses_the_wire_last() {
    let mut capacity = device_capacity(Vec::new());
    capacity.unlearned =
        Some(Refusal::Broken("the node's records can't be read".into()).to_string());
    for offered in [capacity.try_clone().unwrap(), device_capacity(Vec::new())] {
        let offered = Answer::Capacity(Some(offered));
        let bytes = answer(14, &offered, MAX_MESSAGE).unwrap();
        assert_eq!(
            read_answer(&received(&bytes, 0), MAX_MESSAGE).unwrap().1,
            offered
        );
    }
    let mut json = serde_json::to_value(&capacity).unwrap();
    json.as_object_mut().unwrap().remove("unlearned");
    assert_eq!(
        serde_json::from_value::<Capacity>(json).unwrap().unlearned,
        None
    );
}

/// V2 (ROOT, 2026-11-13: a node VM's route's room is its guest): a node VM's receipt carries what its guest is admitted
/// at, after its phase, and a route naming NodeVm crosses the wire by its own tag. Do encode a capacity answer naming a
/// node VM whose guest is admitted at 1.5 CPUs and 3 GiB, and a route offer naming NodeVm, and read both back. Expect
/// each back exactly.
#[test]
fn v2_a_node_vms_receipt_carries_its_guests_room_and_its_route_its_own_tag() {
    let mut capacity = device_capacity(Vec::new());
    capacity.node_vms = vec![node_vm_receipt(7, NodeVmPhase::Ready)];
    capacity.routes = vec![RouteOffer {
        key: RouteKey {
            class: Class::Oci,
            image: ImagePlatform {
                os: ImageOs::Linux,
                arch: ImageArch::Arm64,
            },
            execution: Execution::NodeVm,
            gpu: GpuEligibility::None,
        },
        cost: None,
        backing: None,
    }];
    let offered = Answer::Capacity(Some(capacity));
    let bytes = answer(39, &offered, MAX_MESSAGE).unwrap();
    assert_eq!(
        read_answer(&received(&bytes, 0), MAX_MESSAGE).unwrap().1,
        offered
    );
}

/// A stand-in for hex-quic's AES-256-GCM, for the seal's format and rules (docs/design/hex-quic.md Q13), not its cipher:
/// a body sealed by XOR with a key byte, its tag the first 16 bytes of SHA-256 over that byte, the nonce, the bound header
/// and the sealed body, its nonces a counter as hex-quic's are; opened under the peer's byte.
struct ToySeal {
    mine: u8,
    theirs: u8,
    next: std::sync::atomic::AtomicU64,
}

impl ToySeal {
    /// Two ends of one connection: what one seals, the other opens.
    fn pair() -> (Self, Self) {
        let end = |mine, theirs| ToySeal {
            mine,
            theirs,
            next: std::sync::atomic::AtomicU64::new(0),
        };
        (end(0x5a, 0xa5), end(0xa5, 0x5a))
    }

    fn tag(key: u8, nonce: &[u8; NONCE_BYTES], aad: &[u8], sealed: &[u8]) -> [u8; TAG_BYTES] {
        use sha2::Digest;
        let digest = sha2::Sha256::new()
            .chain_update([key])
            .chain_update(nonce)
            .chain_update(aad)
            .chain_update(sealed)
            .finalize();
        let mut tag = [0u8; TAG_BYTES];
        tag.copy_from_slice(&digest[..TAG_BYTES]);
        tag
    }
}

impl Seal for ToySeal {
    fn seal(
        &self,
        aad: &[u8],
        body: &[u8],
        sealed: &mut [u8],
    ) -> Result<([u8; NONCE_BYTES], [u8; TAG_BYTES]), SealFault> {
        let n = self.next.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let mut nonce = [0u8; NONCE_BYTES];
        nonce[4..].copy_from_slice(&n.to_be_bytes());
        for (to, from) in sealed.iter_mut().zip(body) {
            *to = from ^ self.mine;
        }
        Ok((nonce, Self::tag(self.mine, &nonce, aad, sealed)))
    }

    fn open(
        &self,
        aad: &[u8],
        nonce: &[u8; NONCE_BYTES],
        sealed: &[u8],
        tag: &[u8; TAG_BYTES],
        body: &mut [u8],
    ) -> Result<(), SealFault> {
        if Self::tag(self.theirs, nonce, aad, sealed) != *tag {
            return Err(SealFault::Broken);
        }
        for (to, from) in body.iter_mut().zip(sealed) {
            *to = from ^ self.theirs;
        }
        Ok(())
    }
}

/// Every message the wire's tests write that carries a secret, a client's and the node's, with its kind.
fn secret_messages() -> Vec<(u16, Vec<u8>)> {
    let requests = requests()
        .into_iter()
        .map(|(n, r, _)| request(n, &r, MAX_MESSAGE).unwrap());
    let answers = answers()
        .into_iter()
        .map(|(n, a)| answer(n, &a, MAX_MESSAGE).unwrap());
    requests
        .chain(answers)
        .filter(|bytes| carries_secret(bytes))
        .map(|bytes| (u16::from_le_bytes([bytes[12], bytes[13]]), bytes))
        .collect()
}

/// docs/design/hex-quic.md Q13 (the review's E-6; AC-T7.4.1): do seal each message the wire writes that carries a secret
/// (a lease's value, a sign-in's tokens, a capture's and a refresh's answers) at one end of a connection, and open it at
/// the other. Expect each sealed one marked so by its kind alone, its header otherwise its own, longer by its nonce and
/// tag alone, holding none of its body in the clear and saying it carries no secret in the clear; each opened one the
/// message sealed, byte for byte, read back as it was written; and each nonce one end took never taken again.
#[test]
fn q13_a_message_that_carries_a_secret_goes_sealed_and_opens_to_itself() {
    let (here, there) = ToySeal::pair();
    let secrets = secret_messages();
    let kinds: BTreeSet<u16> = secrets.iter().map(|(kind, _)| *kind).collect();
    assert_eq!(
        kinds,
        BTreeSet::from([0x001b, 0x001c, 0x001d, 0x8015]),
        "the leased, captured and refreshed answers and the capture's ask carry secrets"
    );
    let mut nonces = BTreeSet::new();
    for (kind, message) in &secrets {
        let sealed = seal(message, &here).unwrap();
        assert!(super::sealed(&sealed) && !super::sealed(message));
        assert!(
            !carries_secret(&sealed),
            "a sealed message holds no secret in the clear"
        );
        assert_eq!(sealed.len(), message.len() + SEAL_BYTES);
        assert_eq!(&sealed[..12], &message[..12], "its magic and wire identity");
        assert_eq!(u16::from_le_bytes([sealed[12], sealed[13]]), kind | SEALED);
        assert_eq!(&sealed[14..22], &message[14..22], "its id");
        let body = &message[HEADER_BYTES..];
        assert!(
            body.len() < 8 || !sealed.windows(body.len()).any(|w| w == body),
            "kind {kind:#06x}'s body in the clear"
        );
        assert!(nonces.insert(sealed[HEADER_BYTES..HEADER_BYTES + NONCE_BYTES].to_vec()));
        assert_eq!(&open(sealed, Some(&there)).unwrap(), message);
    }
    assert_eq!(nonces.len(), secrets.len());
}

/// docs/design/hex-quic.md Q13 (AC-T7.4.1): do open, at a connection that seals, each message that carries a secret as
/// it came unsealed, and every other message unsealed. Expect each secret refused `Unsealed`, naming its kind, and every
/// other message back as it came; and none of those sealed, their kinds carrying no secret (`NoSecret`).
#[test]
fn q13_a_connection_that_seals_refuses_a_secret_that_came_unsealed() {
    let (here, there) = ToySeal::pair();
    for (kind, message) in secret_messages() {
        assert_eq!(
            open(message, Some(&there)),
            Err(Fault::Seal(SealFault::Unsealed { kind }))
        );
    }
    let plain = requests()
        .into_iter()
        .map(|(n, r, _)| request(n, &r, MAX_MESSAGE).unwrap())
        .chain(
            answers()
                .into_iter()
                .map(|(n, a)| answer(n, &a, MAX_MESSAGE).unwrap()),
        )
        .filter(|bytes| !carries_secret(bytes));
    let mut seen = 0;
    for message in plain {
        let kind = u16::from_le_bytes([message[12], message[13]]);
        assert_eq!(
            seal(&message, &here),
            Err(Fault::Seal(SealFault::NoSecret { kind }))
        );
        assert_eq!(open(message.clone(), Some(&there)).unwrap(), message);
        seen += 1;
    }
    assert!(seen > 100, "the wire's other messages: {seen}");
}

/// docs/design/hex-quic.md Q13 (AC-T7.4.1): do open a sealed lease whose kind was turned to an inspect's, sealed mark
/// kept, and the same sealed lease at a connection that seals nothing. Expect the first refused `NoSecret`, naming the
/// inspect's kind; the second refused `Unsealing`, naming the lease's; and a lease in the clear taken there as it came
/// (a link that seals nothing carries secrets as the node's socket does, each copy wiped: credential-grants.md C7).
#[test]
fn q13_a_sealed_body_is_refused_where_its_kind_or_connection_takes_none() {
    let (here, there) = ToySeal::pair();
    let (kind, lease) = secret_messages().remove(0);
    let sealed = seal(&lease, &here).unwrap();
    let mut inspect = sealed.clone();
    inspect[12..14].copy_from_slice(&(kind::INSPECT | SEALED).to_le_bytes());
    assert_eq!(
        open(inspect, Some(&there)),
        Err(Fault::Seal(SealFault::NoSecret {
            kind: kind::INSPECT
        }))
    );
    assert_eq!(
        open(sealed, None),
        Err(Fault::Seal(SealFault::Unsealing { kind }))
    );
    assert_eq!(open(lease.clone(), None).unwrap(), lease);
}

/// docs/design/hex-quic.md Q13 (AC-T7.4.1): the header is bound to the sealed body. Do flip each bit of a sealed lease
/// in turn, its header's, its nonce's, its body's and its tag's, and cut it short at every length, and open each at the
/// other end. Expect every one refused, typed: `Broken` where its kind still names a secret, the kind's own mark or a
/// secret's kind turned to another caught as such, and each cut `Truncated` or `Broken`; none opened.
#[test]
fn q13_a_seal_whose_header_nonce_body_or_tag_changed_is_refused() {
    let (here, there) = ToySeal::pair();
    let (_, lease) = secret_messages().remove(0);
    let sealed = seal(&lease, &here).unwrap();
    for at in 0..sealed.len() {
        for bit in 0..8 {
            let mut changed = sealed.clone();
            changed[at] ^= 1 << bit;
            match open(changed, Some(&there)) {
                Err(Fault::Seal(SealFault::Broken)) => {}
                Err(Fault::Seal(SealFault::Unsealed { .. } | SealFault::NoSecret { .. }))
                    if (12..14).contains(&at) => {}
                other => panic!("byte {at} bit {bit}: {other:?}"),
            }
        }
    }
    for len in HEADER_BYTES..sealed.len() {
        match open(sealed[..len].to_vec(), Some(&there)) {
            Err(Fault::Truncated { .. } | Fault::Seal(SealFault::Broken)) => {}
            other => panic!("cut at {len}: {other:?}"),
        }
    }
}

/// docs/design/hex-quic.md Q13 (AC-T7.4.1): a sealed message is held to one message's bound. Do seal a lease whose
/// message is exactly the bound less what sealing adds, and one a byte longer. Expect the first sealed at the bound
/// exactly, and the second refused `Oversized`, naming the bound, before anything is sealed.
#[test]
fn q13_a_sealed_message_is_held_to_one_messages_bound() {
    let (here, _) = ToySeal::pair();
    let leased = |len: usize| {
        let leased = Request::Leased {
            run: 1,
            answer: Ok(Lease {
                lease: "lease-1".to_owned(),
                value: Value::new(vec![7; len]),
                life: None,
            }),
        };
        request(2, &leased, MAX_MESSAGE).unwrap()
    };
    let base = leased(0).len();
    let fits = leased(MAX_MESSAGE - SEAL_BYTES - base);
    assert_eq!(seal(&fits, &here).unwrap().len(), MAX_MESSAGE);
    let over = leased(MAX_MESSAGE - SEAL_BYTES - base + 1);
    assert_eq!(
        seal(&over, &here),
        Err(Fault::Oversized {
            bytes: MAX_MESSAGE + 1,
            bound: MAX_MESSAGE
        })
    );
    assert_eq!(here.next.load(std::sync::atomic::Ordering::Relaxed), 1);
}
