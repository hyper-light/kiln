//! AC-T7.4.1: actual Recorded<WithGuest> and Store, with an explicitly absent guest client.
//! Canonical Live is fixture metadata; this is no native launch, readiness or release proof.
use super::*;
use hex_api::sim::SimRuntime;
use hex_api::{
    BackingGrant, BackingPhase, Bounds, CapacityResources, ClassSpec, Execution, GpuEligibility,
    ImageArch, ImageOs, ImagePlatform, LifecycleCause, Network, OciSpec, Rootfs, RouteKey,
    RouteReceipt, RuntimeCost,
};
use hex_runtime::guest::{GuestConfig, GuestRuntime, Side, WithGuest};
use std::sync::atomic::{AtomicUsize, Ordering};

// This existing simulator advertises no Capacity. Its component adds no invented native routes.
impl Recover for SimRuntime {
    fn project_routes(
        &self,
        _: &[PoolRecord],
        _: Option<(Execution, &[DeviceId])>,
        _: &mut hex_runtime::devices::RouteProjection,
    ) -> Result<(), Refusal> {
        Ok(())
    }
    // Nor does it keep native metadata: it has no names to join to the retained cohort.
    fn preflight_retained(
        &self,
        _: &hex_runtime::devices::Records,
        _: &[PoolRecord],
    ) -> Result<(), Refusal> {
        Ok(())
    }
    fn trace(&self, _: &BoxId) -> Result<Option<ProcId>, Refusal> {
        Ok(None)
    }
    fn adopt(&self, _: &BoxSpec, _: Option<ProcId>) -> Result<(), Refusal> {
        Ok(())
    }
    fn end(&self, _: Option<ProcId>) -> Result<(), Refusal> {
        Ok(())
    }
}

fn route(_: &SimRuntime, class: &ClassSpec) -> Result<Side, Refusal> {
    Ok(if class.class() == Class::Process {
        Side::Host
    } else {
        Side::Guest
    })
}

fn payload(id: &str, root: &Path) -> BoxSpec {
    BoxSpec {
        id: BoxId::new(id).unwrap(),
        bounds: Bounds::default(),
        class: ClassSpec::Oci(Box::new(OciSpec {
            rootfs: Rootfs::Dir(root.to_owned()),
            init: Program {
                args: vec!["/bin/true".into()],
                ..Program::default()
            },
            mounts: Vec::new(),
            hostname: None,
            network: Network::Private,
            hosts: Vec::new(),
        })),
        owner: None,
        allocation: None,
        route: None,
        devices: Vec::new(),
        band: 0,
    }
}

struct Fixture {
    dir: tempfile::TempDir,
    node: Arc<Recorded<WithGuest<SimRuntime>>>,
    pool: PoolRecord,
    boots: Arc<AtomicUsize>,
}

/// The guest-backed node alone, under `root`, with nothing retained yet, and its boot count.
fn guest_node(root: &Path) -> (Arc<Recorded<WithGuest<SimRuntime>>>, Arc<AtomicUsize>) {
    let config = GuestConfig {
        binary: root.join("model-shards"),
        boot: hex_runtime::guest::Boot::Kernel {
            kernel: root.join("model-kernel"),
            initrd: root.join("model-initrd"),
        },
        vcpus: 1,
        memory_mib: shards_api::min_memory_mib(std::env::consts::ARCH),
        gpu: false,
        shares: Vec::new(),
        staged: "/run/hex/staged".into(),
    };
    let floor = CapacityResources {
        cpu_milli: 1000,
        mem_bytes: config.memory_mib.checked_shl(20).unwrap(),
        disk_bytes: 0,
    };
    let cost = RuntimeCost {
        id: [0x63; 32],
        floor,
        ceiling: floor,
        quantum: CapacityResources {
            cpu_milli: 1000,
            mem_bytes: 1 << 20,
            disk_bytes: 1,
        },
        overhead: CapacityResources {
            cpu_milli: 1,
            mem_bytes: 1,
            disk_bytes: 0,
        },
        guest_overhead: CapacityResources {
            cpu_milli: 1,
            mem_bytes: 1,
            disk_bytes: 0,
        },
    };
    let boots = Arc::new(AtomicUsize::new(0));
    let counted = Arc::clone(&boots);
    let guest = GuestRuntime::new(
        Arc::new(shards_api::sim::SimGuests::new(move |_| {
            counted.fetch_add(1, Ordering::SeqCst);
            shards_api::End::PowerOff
        })),
        config,
    )
    .recorded_at(root.join("guest"))
    .with_runtime_paths(
        shards_api::RuntimePaths::new(root.join("cache"), root.join("home")).unwrap(),
    )
    // The image class as `hex node` configures its guest (hex-cli's configured_guest).
    .with_images(
        hex_runtime::guest::host_images(),
        Ok(hex_runtime::guest::ImageWaits::new()),
        root.join("image-diagnostics"),
    )
    .with_runtime_cost(cost)
    .unwrap();
    let node = Arc::new(
        Recorded::open(
            WithGuest::configured(SimRuntime::new(), guest, route),
            &root.join("state"),
        )
        .unwrap(),
    );
    node.bind_guest_books().unwrap();
    (node, boots)
}

fn fixture() -> Fixture {
    let dir = tempfile::tempdir().unwrap();
    let (node, boots) = guest_node(dir.path());
    let prepared = node.prepare_guest_backing().unwrap();
    assert_eq!(prepared.phase, BackingPhase::Prepared);
    let mut pool = node
        .books
        .store
        .grant_pool(&BackingGrant {
            key: prepared.key,
            allocation: prepared.allocation().unwrap(),
            devices: Vec::new(),
        })
        .unwrap();
    // Establish only valid Store fixture transitions, without asserting any native observation.
    pool.init = Some(ProcId {
        pid: 11,
        start_ticks: 13,
    });
    node.books.store.put_pool(&pool).unwrap();
    pool.phase = BackingPhase::Live;
    node.books.store.put_pool(&pool).unwrap();
    pool.check().unwrap();
    Fixture {
        dir,
        node,
        pool,
        boots,
    }
}

/// Original symptom: after a shared create returns uncertainty, its exact Creating row must
/// retain the same backing reference, just as the outer Recorded contract requires elsewhere.
#[test]
fn original_shared_create_uncertainty_keeps_outer_box_and_same_pool_reference() {
    let Fixture {
        dir,
        node,
        pool,
        boots,
    } = fixture();
    let root = dir.path();
    let before = pool.encode().unwrap();
    let mut spec = payload("shared-outer-original", root);
    spec.route = Some(RouteReceipt {
        key: RouteKey {
            class: Class::Oci,
            image: ImagePlatform {
                os: ImageOs::Linux,
                arch: if cfg!(target_arch = "aarch64") {
                    ImageArch::Arm64
                } else {
                    ImageArch::Amd64
                },
            },
            execution: Execution::ShardsShared,
            gpu: GpuEligibility::None,
        },
        backing: Some(pool.key),
    });
    spec.check().unwrap();
    check_backing_reference(
        &BoxRecord::new(spec.clone(), Phase::Creating, Vec::new()),
        &pool,
    )
    .unwrap();
    assert_eq!(node.runtime.route_for(&spec).unwrap(), Side::Guest);
    assert!(!node.runtime.guest().owns_record(&spec));
    assert_eq!(boots.load(Ordering::SeqCst), 0);
    let error = node.create(spec.clone()).unwrap_err();
    assert!(
        matches!(error, Refusal::Backing { key, cause: LifecycleCause::Uncertain, .. } if key == pool.key),
        "unexpected precondition refusal: {error:?}"
    );
    assert_eq!(
        boots.load(Ordering::SeqCst),
        0,
        "this original must never boot a guest"
    );
    assert_eq!(
        node.books
            .store
            .read_pool(&pool.key.pool)
            .unwrap()
            .unwrap()
            .encode()
            .unwrap(),
        before
    );
    let retained = node.books.store.read_lifecycle(&spec.id).unwrap();
    assert!(
        retained.is_some(),
        "shared create uncertainty lost the outer canonical Box/Pool reference"
    );
    let retained = retained.unwrap();
    assert_eq!(retained.phase, Phase::Creating);
    assert_eq!(retained.spec, spec);
    // The native VM scope belongs to the SAME shared Pool, not to each CPU payload.
    assert!(retained.native_scope.is_none());
    assert!(retained.spec.allocation.is_none());
    assert_eq!(retained.spec.route.unwrap().backing, Some(pool.key));
    assert!(retained.init.is_none() && retained.exit.is_none());
    let references = node.books.store.all().unwrap();
    assert_eq!(references.values().filter(|row| matches!(row, Ok(row) if row.spec.route.and_then(|r| r.backing) == Some(pool.key))).count(), 1);
    assert!(matches!(node.create(spec.clone()), Err(Refusal::Exists(id)) if id == spec.id));
    assert_eq!(boots.load(Ordering::SeqCst), 0);
}

/// The dedicated owner remains the sole writer: its rejected native create leaves one
/// Creating row rather than a duplicate outer method turn or unrecorded submission.
#[test]
fn dedicated_owner_refusal_keeps_one_canonical_box_without_outer_reentry() {
    let Fixture {
        dir,
        node,
        pool,
        boots,
    } = fixture();
    let before = pool.encode().unwrap();
    let mut spec = payload("dedicated-owner-control", dir.path());
    let ClassSpec::Oci(oci) = spec.class else {
        panic!("fixture payload class changed")
    };
    spec.class = ClassSpec::Shards(oci);
    spec.bounds.cpu_max_milli = Some(
        u32::try_from(
            pool.cost
                .floor
                .cpu_milli
                .checked_sub(pool.cost.guest_overhead.cpu_milli)
                .unwrap(),
        )
        .unwrap(),
    );
    spec.bounds.memory_max = Some(
        pool.cost
            .floor
            .mem_bytes
            .checked_sub(pool.cost.guest_overhead.mem_bytes)
            .unwrap(),
    );
    spec.allocation = Some(pool.offer().unwrap().allocation().unwrap());
    spec.route = Some(RouteReceipt {
        key: RouteKey {
            class: Class::Shards,
            image: ImagePlatform {
                os: ImageOs::Linux,
                arch: if cfg!(target_arch = "aarch64") {
                    ImageArch::Arm64
                } else {
                    ImageArch::Amd64
                },
            },
            execution: Execution::ShardsDedicated,
            gpu: GpuEligibility::None,
        },
        backing: None,
    });
    spec.check().unwrap();
    assert!(node.runtime.guest().owns_record(&spec));
    let error = node.create(spec.clone()).unwrap_err();
    assert!(
        !matches!(
            error,
            Refusal::Lifecycle {
                cause: LifecycleCause::Busy,
                ..
            }
        ),
        "duplicate outer turn: {error:?}"
    );
    let retained = node
        .books
        .store
        .read_lifecycle(&spec.id)
        .unwrap()
        .expect("dedicated owner must retain its Creating record");
    assert_eq!(retained.phase, Phase::Creating);
    assert_eq!(retained.spec, spec);
    assert!(retained.native_scope.is_some());
    assert_eq!(node.books.store.all().unwrap().len(), 1);
    assert_eq!(boots.load(Ordering::SeqCst), 0);
    assert_eq!(
        node.books
            .store
            .read_pool(&pool.key.pool)
            .unwrap()
            .unwrap()
            .encode()
            .unwrap(),
        before
    );
}

/// AC-T7.4.1: the guest keeps a selected receipt per dedicated box, named by its id. One that no
/// retained record names is an orphan: its native scope is nobody else's to find. The startup barrier
/// refuses it, typed, before any recovery effect, instead of leaving it to fail later as Uncertain.
#[test]
fn an_orphaned_guest_receipt_is_refused_at_the_startup_barrier() {
    let dir = tempfile::tempdir().unwrap();
    let (node, boots) = guest_node(dir.path());
    node.preflight_recovery().unwrap();
    let receipts = dir.path().join("guest.gpu");
    std::fs::create_dir_all(&receipts).unwrap();
    std::fs::write(receipts.join("orphaned-box"), b"retained receipt bytes").unwrap();
    let before = node.books.store.all().unwrap();
    let refused = node.preflight_recovery();
    assert!(
        matches!(
            &refused,
            Err(Refusal::Lifecycle { id, cause: LifecycleCause::Uncertain, .. }) if id.as_str() == "orphaned-box"
        ),
        "an orphaned guest receipt passed the startup barrier: {refused:?}"
    );
    assert_eq!(node.books.store.all().unwrap(), before);
    assert_eq!(boots.load(Ordering::SeqCst), 0);
    assert_eq!(
        std::fs::read(receipts.join("orphaned-box")).unwrap(),
        b"retained receipt bytes"
    );
}

/// Joint plan step 12, T-T7.1.2 (AC-T7.4.1): a node binds its guest's image class to the session its own Store stamps
/// records with, on every OS (none where the Store reads none). Do: bind a guest-backed node's books, then bind the
/// guest's session again as the Store's stamp, and as another session. Expect: the Store's taken again, the one bound;
/// another refused as a rebinding.
#[test]
fn a_nodes_image_class_takes_this_boot_from_its_stores_own_stamp() {
    let dir = tempfile::tempdir().unwrap();
    let (node, _boots) = guest_node(dir.path());
    let stamp = node.books.store.boot().cloned();
    let guest = node.runtime.guest();
    assert_eq!(guest.bind_boot_session(stamp.clone()), Ok(()));
    let other =
        Some(hex_core::record::BootSession::new("00000000-0000-0000-0000-000000000000").unwrap());
    assert_ne!(stamp, other);
    assert!(matches!(
        guest.bind_boot_session(other),
        Err(Refusal::Broken(_))
    ));
}

/// This OS's socket bounds for a node, as `hex node` takes them.
fn socket_limits() -> crate::serve::Limits {
    #[cfg(target_os = "linux")]
    let platform = crate::sizes::Sizes::now().unwrap().platform();
    #[cfg(target_os = "macos")]
    let platform = crate::serve::macos::platform().unwrap();
    #[cfg(windows)]
    let platform = crate::serve::windows::platform().unwrap();
    crate::serve::limits(&platform)
}

// The simulator served bare, its image VMs' records its own, as a node's guest runtime keeps an image VM's
// (`owns_records`): a refused create leaves nothing behind. It ends no exec for a closed connection, and keeps no
// network namespaces.
impl crate::serve::Host for SimRuntime {
    fn abandoned(&self, _: &BoxId) -> Result<(), Refusal> {
        Ok(())
    }
    fn netns(&self) -> Result<hex_core::netns::Counts, Refusal> {
        Ok(hex_core::netns::Counts::default())
    }
}

/// Joint plan step 12, AC-T7.4.1 (ROOT's r558, HEX-IMG-7): a real node's image cases run through its socket, as
/// `image_real` runs them, where a create the client can't send (a grant the wire's check refuses) is refused before
/// any node sees it. Do run every image case through a socket the simulator serves. Expect: none fails, as none fails
/// in process (`hex_api::sim`'s `the_simulator_passes_the_shards_image_cases`).
#[test]
fn the_image_cases_hold_through_a_nodes_socket() {
    let dir = tempfile::tempdir().unwrap();
    let socket = dir.path().join("node.sock");
    let node = Arc::new(SimRuntime::new());
    let _server = crate::serve::Server::bind(&socket, node, socket_limits(), 2).unwrap();
    let client = hex_client::Client::connect(&socket).unwrap();
    let failures = hex_api::conformance::run_image(&client, &hex_api::sim::SimFixture::ShardsImage);
    assert!(failures.is_empty(), "{failures:#?}");
}

/// Joint plan step 17's remote callers, AC-T7.4.1 (docs/design/shards-image-class.md §8): a member of a Shards image
/// VM that its caller drives is handed over through a node's socket as it is in process (`Runtime::member`): the
/// caller's ends of its stdin, stdout and stderr, and its end. Do: through a socket the simulator serves, create and
/// start a VM whose spec names a member its caller drives, take that member, write to its stdin and close it, and
/// read its stdout to its end; then take it again. Expect: its stdout the bytes written, its end Code(0), waited for
/// as it comes; the second take refused Invalid, as in process.
#[test]
fn a_member_is_handed_over_through_a_nodes_socket() {
    use std::io::{Read as _, Write as _};
    let dir = tempfile::tempdir().unwrap();
    let socket = dir.path().join("node.sock");
    let node = Arc::new(SimRuntime::new());
    let _server = crate::serve::Server::bind(&socket, node, socket_limits(), 2).unwrap();
    let client = hex_client::Client::connect(&socket).unwrap();
    let claude = hex_api::Member {
        kind: hex_api::MemberKind::Harness,
        name: "claude".to_owned(),
    };
    let id = client
        .create(BoxSpec {
            id: BoxId::new("member-over-the-wire").unwrap(),
            bounds: Bounds::default(),
            class: ClassSpec::ShardsImage(Box::new(hex_api::ShardsImage {
                members: vec![hex_api::ImageMember::new(claude.clone())],
                ..hex_api::ShardsImage::new(hex_api::sim::SIM_IMAGE)
            })),
            owner: None,
            allocation: Some(hex_api::sim::sim_vm_allocation().unwrap()),
            route: None,
            devices: Vec::new(),
            band: 0,
        })
        .unwrap()
        .id;
    client.start(&id).unwrap();
    let mut member = client.member(&id, &claude).unwrap();
    let given = member.streams().unwrap();
    let mut stdin = hex_api::OwnedStream::from(given.stdin.unwrap());
    let mut stdout = hex_api::OwnedStream::from(given.stdout.unwrap());
    stdin.write_all(b"a session's line\n").unwrap();
    drop(stdin);
    let mut echoed = String::new();
    stdout.read_to_string(&mut echoed).unwrap();
    assert_eq!(echoed, "a session's line\n");
    assert_eq!(
        member.wait_for(std::time::Duration::MAX).unwrap(),
        Some(hex_api::Exit::Code(0))
    );
    assert!(matches!(
        client.member(&id, &claude),
        Err(Refusal::Invalid { what: "member", .. })
    ));
    client.delete(&id).unwrap();
}

/// Joint plan step 12, AC-T7.4.1 (docs/design/shards-image-class.md §8): a macOS or Windows node holds a Shards image
/// VM in its guest runtime, beside its host runtime, and the VM's guest runs no node of Hex's. Do forward a start, an
/// inspect, a usage, a signal and an exec of an image VM's spec through the node's runtime. Expect: each back to run
/// here, where the guest runtime answers it, never to a guest's node, which no guest holding it would refuse
/// NoSuchBox; and no guest boots.
#[test]
fn an_image_vms_calls_run_here_not_at_a_guests_node() {
    let dir = tempfile::tempdir().unwrap();
    let (node, boots) = guest_node(dir.path());
    let image = BoxSpec {
        id: BoxId::new("image-forwarded").unwrap(),
        bounds: Bounds::default(),
        class: ClassSpec::ShardsImage(Box::new(hex_api::ShardsImage::new(hex_api::sim::SIM_IMAGE))),
        owner: None,
        allocation: Some(hex_api::sim::sim_vm_allocation().unwrap()),
        route: None,
        devices: Vec::new(),
        band: 0,
    };
    let id = image.id.clone();
    for call in [
        Call::Start(id.clone()),
        Call::Inspect(id.clone()),
        Call::Usage(id.clone()),
        Call::Signal {
            id: id.clone(),
            signal: hex_api::Signal::KILL,
        },
        Call::Exec {
            id: id.clone(),
            program: hex_api::Program::of(&["/bin/true"]),
            streams: hex_core::wire::Streams {
                stdin: false,
                stdout: false,
                stderr: false,
            },
        },
    ] {
        let said = format!("{call:?}");
        let routed = match node.runtime.forward_for(
            &image,
            call,
            Stdio::null(),
            Box::new(|_| {}),
            Box::new(|_| {}),
        ) {
            Forward::Back(_) => "back".to_owned(),
            Forward::Taken => "taken".to_owned(),
            Forward::Offered(_) => "offered".to_owned(),
            Forward::Refused(why) => format!("refused: {why}"),
        };
        assert_eq!(routed, "back", "{said}");
    }
    assert_eq!(boots.load(Ordering::SeqCst), 0, "a guest booted");
}
