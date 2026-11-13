//! AC-T7.4.1: the shared router must reach its selected backend for physical device requests.
//! This is a dispatch regression, not evidence of native GPU attachment.
use super::*;

fn backend(_host: &hex_api::sim::SimRuntime, _class: &ClassSpec) -> Result<Side, Refusal> {
    Err(Refusal::Invalid {
        what: "fixture backend route",
        why: "the actual route was reached".into(),
    })
}

/// A valid claim reaches the native/guest decision on every host, without a platform refusal in
/// the shared wrapper. The destination alone has the configuration/authority to attach it.
#[test]
fn physical_claims_reach_the_configured_backend_route_for_create_and_run() {
    let node = WithGuest {
        process: hex_api::sim::SimRuntime::new(),
        oci: GuestRuntime::new(
            Arc::new(shards_api::sim::SimGuests::new(|_| {
                shards_api::End::PowerOff
            })),
            config(),
        ),
        route: backend,
    };
    let spec = BoxSpec {
        id: BoxId::new("router-selected-device").unwrap(),
        bounds: hex_api::Bounds::default(),
        class: ClassSpec::Process,
        owner: None,
        allocation: None,
        route: None,
        devices: vec![hex_api::DeviceId {
            kind: hex_api::DeviceKind::Gpu,
            key: 256,
        }],
    };
    spec.check().unwrap();
    let created = node.create(spec.clone());
    let ran = node.run(spec, Program::of(&["unused"]), Stdio::null());
    for refusal in [created.err(), ran.err()] {
        assert!(
            matches!(
                refusal,
                Some(Refusal::Invalid {
                    what: "fixture backend route",
                    ..
                })
            ),
            "actual route was bypassed: {refusal:?}"
        );
    }
    assert!(node.process.list().unwrap().is_empty());
}

pub(super) struct NativeReporting {
    pub(super) inner: hex_api::sim::SimRuntime,
    pub(super) report: hex_api::Capacity,
}

impl Runtime for NativeReporting {
    fn classes(&self) -> Vec<Class> {
        vec![Class::Process, Class::Oci]
    }
    fn capacity(&self, _: Class) -> Result<Option<hex_api::Capacity>, Refusal> {
        Ok(Some(self.report.try_clone()?))
    }
    fn create(&self, spec: BoxSpec) -> Result<BoxInfo, Refusal> {
        self.inner.create(spec)
    }
    fn start(&self, id: &BoxId) -> Result<(), Refusal> {
        self.inner.start(id)
    }
    fn exec(&self, id: &BoxId, program: Program, stdio: Stdio) -> Result<Box<dyn Proc>, Refusal> {
        self.inner.exec(id, program, stdio)
    }
    fn run(&self, spec: BoxSpec, program: Program, stdio: Stdio) -> Result<Box<dyn Proc>, Refusal> {
        self.inner.run(spec, program, stdio)
    }
    fn signal(&self, id: &BoxId, signal: Signal) -> Result<(), Refusal> {
        self.inner.signal(id, signal)
    }
    fn inspect(&self, id: &BoxId) -> Result<BoxInfo, Refusal> {
        self.inner.inspect(id)
    }
    fn usage(&self, id: &BoxId) -> Result<Usage, Refusal> {
        self.inner.usage(id)
    }
    fn list(&self) -> Result<Vec<BoxInfo>, Refusal> {
        self.inner.list()
    }
    fn delete(&self, id: &BoxId) -> Result<(), Refusal> {
        self.inner.delete(id)
    }
}

/// AC-T7.4.1: configuring the Linux-image guest cannot hide the native Windows OCI device pool.
/// Real HCS attachment is separately validated; these capacity replies are explicit model inputs.
#[test]
fn native_oci_device_capacity_remains_visible_with_a_guest_configured() {
    let native = hex_api::Capacity {
        cost: None,
        routes: Vec::new(),
        device_routes: Vec::new(),
        backings: Vec::new(),
        id: "model-native-pool".into(),
        total: hex_api::CapacityResources::default(),
        free: hex_api::CapacityResources::default(),
        devices: vec![hex_api::DeviceInfo {
            id: hex_api::DeviceId {
                kind: hex_api::DeviceKind::Gpu,
                key: 256,
            },
            model: "fixture-native".into(),
            size: 4096,
            holder: None,
        }],
        unavailable: Vec::new(),
        learned: None,
    };
    let guest = Arc::new(NoGuest {
        backend: Ok(shards_api::Backend::Kvm),
        boots: AtomicUsize::new(0),
    });
    let node = WithGuest {
        process: NativeReporting {
            inner: hex_api::sim::SimRuntime::new(),
            report: native.try_clone().unwrap(),
        },
        oci: GuestRuntime::new(guest.clone(), config()),
        route: |_runtime, _class| Ok(Side::Host),
    };
    assert_eq!(node.capacity(Class::Oci).unwrap(), Some(native));
    assert_eq!(guest.boots.load(Ordering::SeqCst), 0);
}

/// AC-T7.4.1: the public seam recognizes the existing KVM/HVF/WHP implementations on every
/// supported host. Hardware capability/boot checks remain with the real backend; Intel Mac excluded.
#[test]
fn every_supported_host_reaches_its_existing_shards_backend() {
    for (os, arch, backend) in [
        ("linux", "x86_64", shards_api::Backend::Kvm),
        ("linux", "aarch64", shards_api::Backend::Kvm),
        ("macos", "aarch64", shards_api::Backend::HypervisorFramework),
        (
            "windows",
            "x86_64",
            shards_api::Backend::WindowsHypervisorPlatform,
        ),
        (
            "windows",
            "aarch64",
            shards_api::Backend::WindowsHypervisorPlatform,
        ),
    ] {
        assert_eq!(shards_api::backend_of(os, arch), Ok(backend), "{os}/{arch}");
    }
    assert!(matches!(
        shards_api::backend_of("macos", "x86_64"),
        Err(shards_api::Refusal::NoBackend { .. })
    ));
}

/// AC-T7.4.1: the conformance suite must agree with supported-host backend recognition.
#[test]
fn conformance_accepts_every_supported_host_backend() {
    let guests = shards_api::sim::SimGuests::new(|_| shards_api::End::PowerOff);
    let serving = config().spec();
    let subject = shards_api::conformance::Subject {
        guests: &guests,
        serving: serving.clone(),
        port: stream::CONTROL_PORT,
        unserved: stream::STDIO_PORT,
        powering_off: serving,
        patience: Duration::ZERO,
    };
    assert_eq!(
        shards_api::conformance::hosts_without_a_backend_are_named(&subject),
        Ok(())
    );
}

/// AC-T7.4.1: backend-explicit Shards cannot be hidden as Windows native OCI;
/// ordinary configured routing still owns native OCI and process work.
#[test]
fn exact_shards_bypasses_native_ordinary_route_without_relabeling_oci() {
    let runtime = WithGuest {
        process: hex_api::sim::SimRuntime::new(),
        oci: GuestRuntime::new(
            Arc::new(shards_api::sim::SimGuests::new(|_| {
                shards_api::End::PowerOff
            })),
            config(),
        ),
        route: |_host, _class| Ok(Side::Host),
    };
    let payload = match oci(Rootfs::Dir("/rootfs".into()), Vec::new()).class {
        ClassSpec::Oci(payload) => payload,
        _ => panic!("fixture must supply an OCI payload"),
    };
    assert_eq!(
        runtime.route(&ClassSpec::Oci(payload.clone())),
        Ok(Side::Host)
    );
    assert_eq!(runtime.route(&ClassSpec::Shards(payload)), Ok(Side::Guest));
    assert_eq!(runtime.route(&ClassSpec::Process), Ok(Side::Host));
    assert!(
        !runtime.oci.classes().contains(&Class::Shards),
        "uninitialized Books/cost is no capability"
    );
}

/// AC-T7.4.1: post-Arc native owner registration cannot require an earlier,
/// unreserved VM boot. Backend-explicit Shards still routes to the guest while
/// Windows/native OCI retains the caller's configured host route.
#[test]
fn configured_owner_route_is_pure_until_native_admission_and_registration() {
    let guests = Arc::new(NoGuest {
        backend: Ok(shards_api::Backend::Kvm),
        boots: AtomicUsize::new(0),
    });
    let runtime = WithGuest::configured(
        hex_api::sim::SimRuntime::new(),
        GuestRuntime::new(guests.clone(), config()),
        |_host, _class| Ok(Side::Host),
    );
    let mut spec = oci(hex_api::Rootfs::Dir("/rootfs".into()), Vec::new());
    assert_eq!(runtime.route(&spec.class).unwrap(), Side::Host);
    let ClassSpec::Oci(payload) = spec.class else {
        panic!("OCI model")
    };
    spec.class = ClassSpec::Shards(payload);
    assert_eq!(runtime.route(&spec.class).unwrap(), Side::Guest);
    assert_eq!(guests.boots.load(Ordering::SeqCst), 0);
    assert!(!runtime.guest().uses_books());
}
