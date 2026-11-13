//! The wire format's tests (decision 3, HEX-SOCK-1 and HEX-SOCK-2): every request and answer reads
//! back as written; a message of another wire is refused before its body is read, naming both wires, and a
//! hello's head names its sender's build whatever the wire; the wire's identity is its definitions' digest,
//! line endings aside; and hostile bytes, at every truncation, every bit flip and at random, are refused by
//! type and never panic.

use super::*;
use hex_api::{Bounds, BoxOwner, BoxSpec, IoRates, Mount, Network, OciSpec, Program, Rootfs, User};
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
    }
}

fn device_capacity(devices: Vec<DeviceInfo>) -> Capacity {
    Capacity {
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
    // additionally ends with four empty u32 lists and no learned use (joint plan step 14); a spec
    // ends with one route option.
    let following = if offer {
        1 + 4 * size_of::<u32>() + 1
    } else {
        2
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
/// only what the node learned (joint plan step 14). Do: encode a capacity listing one of its two devices unavailable;
/// cut the same capacity, listing none and no learned use, before that list's count; read an
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
        cut[cut.len() - 5..],
        [0; 5],
        "the empty list's count, then no learned use, ends it"
    );
    cut.truncate(cut.len() - 5);
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
        },
        Refusal::Lifecycle {
            id: id("retained-box"),
            cause: LifecycleCause::Uncertain,
        },
        Refusal::Lifecycle {
            id: id("retained-box"),
            cause: LifecycleCause::Busy,
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
        }),
    ));
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

/// AC-T7.4.1: retained-resource lifecycle causes survive the wire as distinct typed outcomes;
/// an unknown cause cannot become absence, success or a default retry decision.
#[test]
fn lifecycle_causes_preserve_box_identity_and_reject_unknown_tags() {
    for cause in [
        LifecycleCause::Pending,
        LifecycleCause::Uncertain,
        LifecycleCause::Busy,
    ] {
        let refused = Answer::Refused(Refusal::Lifecycle {
            id: id("retained-box"),
            cause,
        });
        let bytes = answer(41, &refused, MAX_MESSAGE).unwrap();
        assert_eq!(
            read_answer(&received(&bytes, 0), MAX_MESSAGE).unwrap().1,
            refused
        );
        for tag in [3, u8::MAX] {
            let mut invalid = bytes.clone();
            *invalid.last_mut().unwrap() = tag;
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
/// and the stream module's, with no number to bump, and a checkout's line endings don't split it. Do: digest the two
/// files as read now, then with every line ended `\r\n`. Expect: both [`IDENTITY`]; and a source with one byte more
/// another identity.
#[test]
fn the_wire_identity_is_its_definitions_digest_line_endings_aside() {
    let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
    let paths = [
        "wire.rs",
        "stream.rs",
        "schedule.rs",
        "../../hex-sim/src/peer.rs",
        "../../hex-sim/src/model.rs",
        "../../hex-api/src/spec.rs",
        "../../hex-api/src/id.rs",
        "../../hex-api/src/bounds.rs",
        "../../hex-api/src/devices.rs",
        "../../hex-api/src/runtime.rs",
        "../../hex-api/src/grants.rs",
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
    for kind in [0u16, 0x0017, 0x7fff, 0x8001, 0x8009, 0xffff] {
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
    // empty grant and published lists' counts and the last resort's none tag end the info.
    let adopted = info.len() - NO_GRANTS - NO_END - 2 - 8 - 6 - 1;
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
            ])
        )
            .prop_map(|(id, cause)| Refusal::Lifecycle { id, cause }),
        (any::<u32>(), any_connect_failure())
            .prop_map(|(port, failure)| Refusal::Connect { port, failure }),
        proptest::sample::select(Upstream::ALL.to_vec())
            .prop_map(|needs| Refusal::Awaits { needs }),
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
                Execution::ShardsShared => GpuEligibility::None,
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
            Execution::Native | Execution::Firecracker => None,
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
    // end it.
    prefix.truncate(prefix.len() - 4 * size_of::<u32>() - 1);
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
    });
    let bytes = answer(203, &expected, MAX_MESSAGE).unwrap();
    assert_eq!(&bytes[12..14], &0x8005u16.to_le_bytes());
    // The counters, then guest time's option byte, appended (joint plan step 12): none here.
    assert_eq!(bytes.len(), HEADER_BYTES + size_of::<[u64; 8]>() + 1);
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
        assert_eq!(plain.last(), Some(&0));
        assert_eq!(&plain[..plain.len() - 1], &bytes[..plain.len() - 1]);
        assert_eq!(bytes[plain.len() - 1], 1);

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
        });
        let bytes = answer(207, &expected, MAX_MESSAGE).unwrap();
        assert_eq!(bytes.len(), HEADER_BYTES + 1 + BACKING_KEY_BYTES + 1);
        assert_eq!(bytes[HEADER_BYTES], 10);
        assert_eq!(bytes.last(), Some(&tag));
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
    // end it.
    prefix.truncate(prefix.len() - 4 * size_of::<u32>() - 1);
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
        }),
        MAX_MESSAGE,
    )
    .unwrap();
    *bytes.last_mut().unwrap() = 3;
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
        let minimum = if grant {
            BACKING_GRANT_MIN_BYTES
        } else {
            BACKING_OFFER_MIN_BYTES
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
    let route_bytes = 1 + ROUTE_KEY_BYTES + 1 + BACKING_KEY_BYTES;
    let route_at = bad.len() - route_bytes;
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
    // The info's route comes before its empty grant and published lists' counts and its last resort's none tag.
    let route_at = bad.len() - NO_GRANTS - NO_END - route_bytes;
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
        });
    }
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
        },
    )
    .unwrap()
}

/// Qualified observer failures append to, rather than replace, actual native identity and load.
#[test]
fn physical_report_unavailable_roundtrips_qualified_backing_causes() {
    assert_eq!(PHYSICAL_UNAVAILABLE_BYTES, 51);
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
    for cause in [3, u8::MAX] {
        let mut raw = bytes.clone();
        raw[entry + PHYSICAL_UNAVAILABLE_BYTES - 1] = cause;
        assert!(matches!(
            refused(&raw),
            Fault::Value {
                what: "backing.cause",
                ..
            }
        ));
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
    assert_eq!(bytes.len(), counters + 6 + 2 * size_of::<u64>());
    assert_eq!(
        read_answer(&received(&bytes, 0), MAX_MESSAGE).unwrap().1,
        per_vcpu
    );
    let awaits = usage(Some(GuestTime::Awaits(Upstream::GuestCpuTimePerVcpu)));
    let bytes = answer(4, &awaits, MAX_MESSAGE).unwrap();
    assert_eq!(&bytes[counters..], &[1, 1, 0]);
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
    read_past.truncate(read_past.len() - size_of::<u32>());
    read_past.extend_from_slice(&count.to_le_bytes());
    read_past.extend(std::iter::repeat_n(
        0,
        (GuestTime::MAX_VCPUS + 1) * size_of::<u64>(),
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
        id: BoxId::new(id).unwrap(),
        holding: learned_resources(cpu_milli, gib),
    }
}

fn learned_end(id: &str, gib: u64) -> LearnedEnd {
    LearnedEnd {
        id: BoxId::new(id).unwrap(),
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
    });
    capacity
}

/// Joint plan step 14 (AC-T7.4.1, C2, C5 and C8): what a node learned of a class's microVMs crosses the wire last on
/// its capacity answer. Do: encode a capacity with its priors, learned use, holdings and the VMs its last resort
/// ended, and one with none; cut the second before its last byte; read and write both as JSON, and read the first's
/// JSON without its ends. Expect both back exactly, the second ending with its none tag and refused once cut, as every
/// truncation is, JSON without the field reading as none, and JSON without the ends as none ended.
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
    assert_eq!(cut.last(), Some(&0), "no learned use ends it");
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
    }
    let mut bytes = answer(14, &Answer::Capacity(Some(one)), MAX_MESSAGE).unwrap();
    // The holdings' count, then the one holding (its id's length, the id and its resources), then no end's count.
    let at = bytes.len() - (3 * size_of::<u32>() + 1 + 3 * size_of::<u64>());
    let count = u32::try_from(MAX_LEARNED_HOLDINGS + 1).unwrap();
    bytes[at..at + size_of::<u32>()].copy_from_slice(&count.to_le_bytes());
    assert!(matches!(
        read_answer(&received(&bytes, 0), MAX_MESSAGE),
        Err(Fault::Value { .. })
    ));
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
    }
    let mut bytes = answer(14, &Answer::Capacity(Some(one)), MAX_MESSAGE).unwrap();
    // The ends' count, then the one end: its id's length, the id, five u64s, its band and its unmoved capability's
    // option and tag.
    let at = bytes.len() - (2 * size_of::<u32>() + 1 + 5 * size_of::<u64>() + 1 + 2);
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
/// What ends an info its node's last resort didn't end (joint plan step 14): its end's none tag, after the grants.
const NO_END: usize = 1;

/// Joint plan step 14 (AC-T7.4.1, C5): why a node's last resort ended a VM crosses the wire last on its info. Do:
/// encode an info with an end and one with none, cut the second before its last byte, and read and write both as
/// JSON. Expect both back exactly, the second ending with its none tag and refused once cut, as every truncation is,
/// and JSON without the field reading as none.
#[test]
fn w14_an_infos_last_resort_crosses_the_wire_last() {
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
    assert_eq!(cut.last(), Some(&0), "no end's tag ends it");
    cut.truncate(cut.len() - NO_END);
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
/// the host bound it. Expect: read back whole, the bound port's grant, socket and receiver last but for the last
/// resort's none tag (joint plan step 14). Do: answer one
/// whose report names an egress grant, and read one whose receiver tag no receiver has. Expect: refused as
/// "published" written, and by the receiver's name read.
#[test]
fn an_inspect_carries_its_grants_and_their_ports_as_bound() {
    let published = Published {
        grant: 2,
        at: "127.0.0.1:49152".parse().unwrap(),
        receiver: Receiver::Declared,
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
    tail.push(0);
    tail.extend([0; NO_END]);
    assert!(bytes.ends_with(&tail));
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
    let receiver = hostile.len() - 1 - NO_END;
    hostile[receiver] = 1;
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
