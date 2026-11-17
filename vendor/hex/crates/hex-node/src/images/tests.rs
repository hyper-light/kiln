//! The image class alone as a runtime (joint plan step 12's item B), over a model of Shards' image boot whose runs hold
//! until their stop: what its boxes' records are, where they are kept, and what a restart of its caller finds.

use std::sync::{Condvar, Mutex};

use hex_api::{Allocation, Bounds, BoxState, ClassSpec, ShardsImage};
use shards_api::image::{
    Bound, Channel, ChannelEnd, ImageGuest, ImageRefusal, ImageSpec, MemberEnd, MemberInfo,
    MemberName, Ready, RunEnd, RunEnded, StartedFrom, Stats, StatsScope, Unused,
};

use super::*;

/// Shards' image boot in the model: each boot answered at once with a run that holds until its stop, but while the
/// test holds the model's boots, when each waits until it lets them go, and where the test has it refuse the next.
#[derive(Default)]
pub(crate) struct Model {
    asked: Mutex<Vec<ImageSpec>>,
    held: Mutex<bool>,
    let_go: Condvar,
    refuse: Mutex<Option<ImageRefusal>>,
}

impl Model {
    /// Holds each boot from now on (`true`), or lets every one held go (`false`).
    fn hold(&self, held: bool) {
        *self.held.lock().unwrap() = held;
        self.let_go.notify_all();
    }
}

impl Images for Model {
    fn backend(&self) -> Result<shards_api::Backend, ImageRefusal> {
        Ok(shards_api::Backend::Kvm)
    }

    fn boot(&self, spec: &ImageSpec, _: Duration) -> Result<Box<dyn ImageGuest>, ImageRefusal> {
        self.asked.lock().unwrap().push(spec.clone());
        drop(
            self.let_go
                .wait_while(self.held.lock().unwrap(), |held| *held)
                .unwrap(),
        );
        if let Some(why) = self.refuse.lock().unwrap().take() {
            return Err(why);
        }
        Ok(Box::new(Held::default()))
    }
}

/// A run of the model's: no member, nothing published, its end `Stopped` once stopped.
#[derive(Default)]
struct Held {
    end: Mutex<Option<RunEnd>>,
}

fn unasked() -> ImageRefusal {
    ImageRefusal::Unsupported {
        what: "the model's run",
        why: "these tests never ask it".to_owned(),
    }
}

impl ImageGuest for Held {
    fn members(&self) -> &[MemberInfo] {
        &[]
    }
    fn published(&self) -> Vec<Bound> {
        Vec::new()
    }

    /// It has no network: nothing past its microVM is granted.
    fn egress(&self) -> shards_api::image::Narrowing {
        use shards_api::image::Keep;
        shards_api::image::Narrowing {
            egress: Keep::Only(Vec::new()),
            mcp: Keep::Only(Vec::new()),
            destinations: Keep::Only(Vec::new()),
            dns: false,
            members: None,
        }
    }
    fn grant_egress(
        &self,
        _: &shards_api::image::EgressGrant,
        _: Duration,
    ) -> Result<shards_api::image::Narrowing, ImageRefusal> {
        Err(unasked())
    }
    fn revoke_egress(
        &self,
        _: &shards_api::image::EgressGrant,
        _: Duration,
    ) -> Result<shards_api::image::Narrowing, ImageRefusal> {
        Err(unasked())
    }
    fn member_egress(
        &self,
        _: &MemberName,
        _: Option<&shards_api::image::Reach>,
        _: Duration,
    ) -> Result<shards_api::image::Narrowing, ImageRefusal> {
        Err(unasked())
    }

    fn publish(
        &self,
        _: shards_api::image::Publish,
        _: Duration,
    ) -> Result<Vec<Bound>, ImageRefusal> {
        Err(unasked())
    }

    fn unpublish(
        &self,
        _: shards_api::image::Proto,
        _: std::net::SocketAddr,
        _: Duration,
    ) -> Result<(), ImageRefusal> {
        Err(unasked())
    }

    fn pause(&self, _: Duration) -> Result<shards_api::image::Paused, ImageRefusal> {
        Err(unasked())
    }

    fn resume(&self, _: Duration) -> Result<shards_api::image::Paused, ImageRefusal> {
        Err(unasked())
    }
    fn control(&self) -> Option<&shards_api::image::ControlAt> {
        None
    }
    fn rate_period(&self) -> Option<Duration> {
        Some(Duration::from_millis(100))
    }
    fn started_from(&self) -> StartedFrom {
        StartedFrom::Booted
    }
    fn take_output(
        &self,
        _: &MemberName,
        _: u32,
        _: Channel,
    ) -> Result<std::io::PipeReader, ImageRefusal> {
        Err(unasked())
    }
    fn take_input(&self, _: &MemberName, _: u32) -> Result<std::io::PipeWriter, ImageRefusal> {
        Err(unasked())
    }
    fn wait_member(
        &self,
        _: &MemberName,
        _: u32,
        _: Duration,
    ) -> Result<Option<MemberEnd>, ImageRefusal> {
        Err(unasked())
    }
    fn restart(&self, _: &MemberName, _: &[Vec<u8>], _: Duration) -> Result<u32, ImageRefusal> {
        Err(unasked())
    }
    fn swap(
        &self,
        _: &str,
        _: &str,
        _: shards_api::image::SwapWhen,
        _: Option<&[Vec<u8>]>,
        _: Duration,
    ) -> Result<shards_api::image::Swapped, ImageRefusal> {
        Err(unasked())
    }
    fn restore(
        &self,
        _: &str,
        _: shards_api::image::SwapWhen,
        _: Option<&[Vec<u8>]>,
        _: Duration,
    ) -> Result<shards_api::image::Swapped, ImageRefusal> {
        Err(unasked())
    }
    /// None swapped: it runs its image's own.
    fn components(&self) -> Vec<shards_api::image::ComponentAt> {
        Vec::new()
    }
    fn stats(&self, _: StatsScope, _: Duration) -> Result<Stats, ImageRefusal> {
        Err(unasked())
    }
    fn room(&self, _: Option<i64>, _: Duration) -> Result<Option<u64>, ImageRefusal> {
        Err(unasked())
    }
    fn rate(
        &self,
        _: Option<std::num::NonZeroU64>,
        _: Duration,
    ) -> Result<Option<std::num::NonZeroU64>, ImageRefusal> {
        Err(unasked())
    }
    fn resize(&self, _: &MemberName, _: u16, _: u16, _: Duration) -> Result<(), ImageRefusal> {
        Err(unasked())
    }
    fn wait_ready(&self, _: Option<Ready>, _: Duration) -> Result<Option<Ready>, ImageRefusal> {
        Err(unasked())
    }
    fn plug(&self, _: &shards_api::image::Plug, _: Duration) -> Result<(), ImageRefusal> {
        Err(unasked())
    }
    fn unplug(&self, _: &str, _: Duration) -> Result<(), ImageRefusal> {
        Err(unasked())
    }
    fn save(&self, _: &std::path::Path, _: Duration) -> Result<(), ImageRefusal> {
        Err(unasked())
    }
    fn stop(&self) -> Result<(), ImageRefusal> {
        self.end.lock().unwrap().get_or_insert(RunEnd::Stopped);
        Ok(())
    }
    fn wait_for(&self, _: Duration) -> Result<Option<RunEnded>, ImageRefusal> {
        Ok(self
            .end
            .lock()
            .unwrap()
            .clone()
            .map(|end| RunEnded::unused(end, Unused::Killed)))
    }
    fn keep_diagnostics(&self, _: std::fs::File, _: u64) -> Result<(), ImageRefusal> {
        Ok(())
    }
    fn wait_channel(
        &self,
        _: &MemberName,
        _: u32,
        _: Channel,
        _: Duration,
    ) -> Result<Option<ChannelEnd>, ImageRefusal> {
        Err(unasked())
    }
    fn take_gpu_report(
        &self,
        _: Duration,
    ) -> Result<shards_api::physical::GpuReport, ImageRefusal> {
        Err(unasked())
    }
}

/// kiln's id for a reserved run at its largest: `kiln-r<epoch>-<seq>`, each a u64 in decimal (goal 54's change 5).
const RESERVED: &str = "kiln-r18446744073709551615-18446744073709551615";

fn allocation() -> Allocation {
    hex_api::sim::sim_vm_allocation().unwrap()
}

pub(crate) fn image_box(id: &str) -> BoxSpec {
    BoxSpec {
        id: BoxId::new(id).unwrap(),
        bounds: Bounds::default(),
        class: ClassSpec::ShardsImage(Box::new(ShardsImage::new(hex_api::sim::SIM_IMAGE))),
        owner: None,
        devices: Vec::new(),
        allocation: Some(allocation()),
        route: None,
        band: 0,
    }
}

/// An image runtime over the model, its records and Shards home and cache under `dir`.
fn runtime(dir: &Path, model: &Arc<Model>) -> Arc<ImageRuntime> {
    let paths = shards_api::RuntimePaths::new(dir.join("cache"), dir.join("home")).unwrap();
    ImageRuntime::with_images(
        dir.join("shards"),
        paths,
        &dir.join("records"),
        Ok(Arc::clone(model) as Arc<dyn Images>),
        Ok(ImageWaits::new()),
    )
    .unwrap()
}

/// AC-T7.4.1 (kiln plan 01a1dd2e…, P2.2): kiln's node, an image runtime, lists the Shards image class only where it
/// boots image VMs, and one that can't carries Shards' why. Do: open an image runtime over the model's image boot, and
/// one over the refusal `host_images` makes where Shards' check says this host runs no microVM; create an image VM on
/// each. Expect: the first lists shards-image alone and makes it, of that class; the second lists none, says why as its
/// class serves none, and refuses the create Platform in Shards' words, keeping no record and asking no boot.
#[test]
fn p2_2_an_image_runtime_lists_its_class_only_where_it_boots_image_vms() {
    let dir = tempfile::tempdir().unwrap();
    let model = Arc::new(Model::default());
    let serving = runtime(&dir.path().join("serving"), &model);
    assert_eq!(serving.classes(), vec![Class::ShardsImage]);
    assert_eq!(serving.class().serves(), Ok(()));
    let made = serving.create(image_box("served")).unwrap();
    assert_eq!(made.class, Class::ShardsImage);
    let why = Refusal::Platform {
        what: "Shards image VMs",
        platform: std::env::consts::OS,
        why: shards_api::Unusable::KvmDenied.to_string(),
    };
    let unserved = ImageRuntime::with_images(
        dir.path().join("shards"),
        shards_api::RuntimePaths::new(dir.path().join("cache"), dir.path().join("home")).unwrap(),
        &dir.path().join("unserved"),
        Err(why.clone()),
        Ok(ImageWaits::new()),
    )
    .unwrap();
    assert_eq!(unserved.classes(), Vec::<Class>::new());
    assert_eq!(unserved.class().serves(), Err(why.clone()));
    assert_eq!(unserved.create(image_box("refused")), Err(why));
    assert_eq!(unserved.list().unwrap(), Vec::new());
    assert!(model.asked.lock().unwrap().is_empty());
}

/// The second box shown Running once its held boot is let go: this test's fact alone.
static SHOWN: hex_core::patience::Patience =
    hex_core::patience::Patience::measuring("test.hex_node.image_start_shown");

/// Joint plan step 12's item B (goal 54's changes 4 and 5; AC-T7.4.1; decision 9): the image class alone serves a node
/// with no guest of Hex's. Do open an image runtime over a records directory of its own; create a box of another class;
/// create, start, inspect and list an image VM named by kiln's largest reservation id; create another and start it, its
/// boot slower than the first's answer (held, as a loaded host's may be), follow it as its caller does, and delete it;
/// then end the runtime and open another over the same records, as kiln's restart does, and list. Expect the runtime to
/// serve the Shards class alone, the other class refused; each VM booted once, the first shown Running and listed under
/// its id byte for byte, as its record in the runtime's own store names it; the second's start Pending at W*, the first
/// boot's answer, its boot going on, and the box shown Running once it answers; its record gone with its delete, every
/// call on it then NoSuchBox; and the restarted runtime to list the first under the same id, held, as an earlier
/// process's run is until its host's reboot proves it ended.
#[test]
fn w12_an_image_runtime_serves_the_image_class_alone_on_a_store_of_its_own() {
    let dir = tempfile::tempdir().unwrap();
    let model = Arc::new(Model::default());
    let images = runtime(dir.path(), &model);
    assert_eq!(images.classes(), vec![Class::ShardsImage]);
    let mut process = image_box("process-box");
    process.class = ClassSpec::Process;
    process.allocation = None;
    assert!(matches!(
        images.create(process),
        Err(Refusal::Invalid { what: "class", .. })
    ));
    let made = images.create(image_box(RESERVED)).unwrap();
    assert_eq!(made.id.as_str(), RESERVED);
    let id = made.id;
    images.start(&id).unwrap();
    assert_eq!(images.inspect(&id).unwrap().state, BoxState::Running);
    let gone = images.create(image_box("kiln-z7-1")).unwrap().id;
    // Its boot slower than the first's answer, as the push gate's load made one (ROOT's run at 312f4e98): its start
    // answers Pending at W*, the boot going on (decision 9), and its caller follows the box to its run.
    model.hold(true);
    assert!(matches!(
        images.start(&gone),
        Err(Refusal::Lifecycle {
            cause: hex_api::LifecycleCause::Pending,
            ..
        })
    ));
    model.hold(false);
    // A look: the image runtime tells no event as a boot's run starts, its inspect read. Its party, the box's boot,
    // ends where the box shows an end instead.
    hex_testkit::waits::follow::until(
        &SHOWN,
        "the second box shown running",
        || {
            if images.inspect(&gone).is_ok_and(|info| info.exit.is_none()) {
                hex_testkit::waits::follow::Party::Live
            } else {
                hex_testkit::waits::follow::Party::Ended
            }
        },
        || images.inspect(&gone).unwrap().state == BoxState::Running,
    )
    .unwrap();
    assert_eq!(model.asked.lock().unwrap().len(), 2);
    images.delete(&gone).unwrap();
    assert!(matches!(images.inspect(&gone), Err(Refusal::NoSuchBox(_))));
    assert!(matches!(images.start(&gone), Err(Refusal::NoSuchBox(_))));
    let listed: Vec<String> = images
        .list()
        .unwrap()
        .into_iter()
        .map(|info| info.id.as_str().to_owned())
        .collect();
    assert_eq!(listed, vec![RESERVED.to_owned()]);
    let kept: Vec<String> = Books::records(images.as_ref())
        .unwrap()
        .into_keys()
        .map(|id| id.as_str().to_owned())
        .collect();
    assert_eq!(kept, vec![RESERVED.to_owned()]);
    drop(images);
    let restarted = runtime(dir.path(), &Arc::new(Model::default()));
    let after: Vec<(String, BoxState)> = restarted
        .list()
        .unwrap()
        .into_iter()
        .map(|info| (info.id.as_str().to_owned(), info.state))
        .collect();
    assert_eq!(after, vec![(RESERVED.to_owned(), BoxState::Running)]);
}

/// Kiln's HexVms adapter (ROOT, 2026-11-13; joint plan step 12, decision 9): kiln drives the image runtime in process,
/// and a start past its boot's W* answers Pending, so kiln waits for the box with `started` rather than polling, and
/// the runtime answers it as its class does. Do: boot one VM, teaching the boot's W*; create another and start it, its
/// boot held past that W* (its start Pending); have its boot refuse, ask `started` and let the boot go. Expect:
/// `started` answering the run's own failure in its runtime's words, `Refusal::Failed`, never the box as it stood.
#[test]
fn w12_the_image_runtime_answers_started_as_its_class_follows_the_boot() {
    let dir = tempfile::tempdir().unwrap();
    let model = Arc::new(Model::default());
    let images = runtime(dir.path(), &model);
    let first = images.create(image_box("kiln-r1-1")).unwrap().id;
    images.start(&first).unwrap();
    let held = images.create(image_box("kiln-r1-2")).unwrap().id;
    model.hold(true);
    assert!(matches!(
        images.start(&held),
        Err(Refusal::Lifecycle {
            cause: hex_api::LifecycleCause::Pending,
            ..
        })
    ));
    *model.refuse.lock().unwrap() = Some(ImageRefusal::Boot(
        "its image's members never started".to_owned(),
    ));
    let answered = std::thread::scope(|scope| {
        let asking = scope.spawn(|| images.started(&held));
        model.hold(false);
        asking.join().unwrap()
    });
    assert!(
        matches!(&answered, Err(Refusal::Failed { why, .. }) if why.contains("its image's members never started")),
        "{answered:?}"
    );
}

/// An allocation of `vcpus` vCPUs and `memory_mib` MiB, its cost's quantum Hex's own (whole vCPUs, MiB).
fn sized(vcpus: u64, memory_mib: u64) -> Allocation {
    let vm = hex_api::CapacityResources {
        cpu_milli: vcpus * 1_000,
        mem_bytes: memory_mib << 20,
        disk_bytes: 0,
    };
    hex_api::RuntimeCost {
        id: [4; 32],
        floor: hex_api::CapacityResources::default(),
        ceiling: vm,
        quantum: hex_api::CapacityResources {
            cpu_milli: 1_000,
            mem_bytes: 1 << 20,
            disk_bytes: 1,
        },
        guest_overhead: hex_api::CapacityResources::default(),
        overhead: hex_api::CapacityResources::default(),
    }
    .allocate(vm)
    .unwrap()
}

/// Joint plan step 12's item B (goal 54's changes 1 and 2; AC-T7.4.1): kiln reserves a box's amount before the box
/// exists, by `shards_api::envelope`, the pure function Shards' own `check` holds a guest to (its unit and least memory,
/// one source), and a create takes the reservation it is passed. Do create an image VM whose allocation is its own
/// envelope; one whose memory isn't (an odd MiB, which Shards boots one MiB up); and one asking less than Shards' least
/// guest, its floor. Expect the first made; each other refused as Invalid before any record, naming the allocation and
/// what Shards would boot for it, as `shards_api::envelope` says it: the second its next unit, the third the floor.
#[test]
fn w12_a_create_takes_its_reservation_as_shards_envelope_says_and_refuses_another() {
    let dir = tempfile::tempdir().unwrap();
    let images = runtime(dir.path(), &Arc::new(Model::default()));
    let arch = std::env::consts::ARCH;
    let (vcpus, mib) = shards_api::envelope(arch, 2, 256).unwrap();
    let mut fits = image_box("kiln-r1-1");
    fits.allocation = Some(sized(u64::from(vcpus), mib));
    images.create(fits).unwrap();
    for (name, asked_mib) in [("kiln-r1-2", 255), ("kiln-r1-3", 2)] {
        let (_, booted_mib) = shards_api::envelope(arch, 1, asked_mib).unwrap();
        assert_ne!(booted_mib, asked_mib);
        let mut past = image_box(name);
        past.allocation = Some(sized(1, asked_mib));
        match images.create(past) {
            Err(Refusal::Invalid {
                what: "shards image",
                why,
            }) => assert!(
                why.contains("allocation") && why.contains(&format!("{booted_mib} MiB")),
                "{name}: {why}"
            ),
            other => panic!("{name}: {other:?}"),
        }
    }
    let kept: Vec<String> = Books::records(images.as_ref())
        .unwrap()
        .into_keys()
        .map(|id| id.as_str().to_owned())
        .collect();
    assert_eq!(kept, vec!["kiln-r1-1".to_owned()]);
}

/// The placeholder kiln's broker gives a harness for its account's credential (Shards' D242): a visible marker, then 256
/// random bits in hex.
const PLACEHOLDER: &str =
    "kiln-placeholder-0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";

/// An image VM whose harness `claude` is granted credential `anthropic/ada` toward its provider, which its egress reaches
/// (joint plan step 33's example).
fn credentialed(id: &str) -> BoxSpec {
    use hex_api::{CredentialGrant, Destination, Grant, Member, MemberKind, Ports, Proto};
    let claude = Member {
        kind: MemberKind::Harness,
        name: "claude".to_owned(),
    };
    let mut spec = image_box(id);
    if let ClassSpec::ShardsImage(image) = &mut spec.class {
        image.grants = vec![Grant::Egress {
            member: claude.clone(),
            to: Destination::Name("api.anthropic.com".to_owned()),
            ports: Ports {
                proto: Proto::Tcp,
                first: 443,
                last: 443,
            },
        }];
        image.credentials = vec![CredentialGrant {
            member: claude,
            reference: "anthropic/ada".to_owned(),
            toward: vec!["api.anthropic.com".to_owned()],
            placeholder: PLACEHOLDER.to_owned(),
        }];
    }
    spec
}

/// AC-T7.7.2, joint plan step 33 (C3; Shards' D242; kiln's vendor patch 0005, taken upstream): an orchestrator linking
/// hex-node gives its image runtime the lessors over its broker. Do: over a broker holding `anthropic/ada`, make a runtime
/// as `ImageRuntime::new` makes one and another as `ImageRuntime::leasing` does on a host whose Shards check says it runs
/// microVMs (their boot Shards' own, as `host_images` gives it there: P2.2), each over records of its own, and create a
/// VM granting that credential in each; then, in a runtime built as `leasing` builds one but over the model's boot, create
/// and start such a VM and lease the credential through the lessor its boot was given. Expect: `new`'s create refused
/// `Unconfigured` before any record; `leasing`'s made, its record holding the grant; and the boot given the box's
/// credential and a lessor whose lease is the broker's value, held there.
#[test]
fn w33_a_runtime_made_leasing_leases_its_vms_credentials_and_one_made_new_refuses_them() {
    use hex_api::lease::sim::SimLessors;
    let broker = SimLessors::new();
    broker.hold("anthropic/ada", b"sk-ant-api03-real").unwrap();
    let paths =
        |dir: &Path| shards_api::RuntimePaths::new(dir.join("cache"), dir.join("home")).unwrap();

    // Shards' image boot, as `host_images` gives it where the host's check passes; `new` and `leasing` ask that
    // check of their `shards` (P2.2: its own tests), which these paths don't name.
    let serving = || Ok(Arc::new(shards_api::image::ImageShards::new()) as Arc<dyn Images>);

    let plain = tempfile::tempdir().unwrap();
    let unleased = ImageRuntime::with_images(
        plain.path().join("shards"),
        paths(plain.path()),
        &plain.path().join("records"),
        serving(),
        Ok(ImageWaits::new()),
    )
    .unwrap();
    assert!(matches!(
        unleased.create(credentialed("kiln-r1-1")),
        Err(Refusal::Unconfigured { what, .. }) if what.contains("credentials")
    ));
    assert!(Books::records(unleased.as_ref()).unwrap().is_empty());

    let leased_dir = tempfile::tempdir().unwrap();
    let leasing = ImageRuntime::built(
        leased_dir.path().join("shards"),
        paths(leased_dir.path()),
        &leased_dir.path().join("records"),
        serving(),
        Ok(ImageWaits::new()),
        Some(Arc::new(broker.clone())),
    )
    .unwrap();
    let spec = credentialed("kiln-r1-1");
    let granted = match &spec.class {
        ClassSpec::ShardsImage(image) => image.credentials.clone(),
        other => panic!("an image VM's spec as {other:?}"),
    };
    let made = leasing.create(spec).unwrap().id;
    assert_eq!(leasing.inspect(&made).unwrap().credentials, granted);

    let booted = tempfile::tempdir().unwrap();
    let model = Arc::new(Model::default());
    let runtime = ImageRuntime::built(
        booted.path().join("shards"),
        paths(booted.path()),
        &booted.path().join("records"),
        Ok(Arc::clone(&model) as Arc<dyn Images>),
        Ok(ImageWaits::new()),
        Some(Arc::new(broker.clone())),
    )
    .unwrap();
    let id = runtime.create(credentialed("kiln-r1-2")).unwrap().id;
    runtime.start(&id).unwrap();
    let asked = model.asked.lock().unwrap().clone();
    assert_eq!(asked.len(), 1);
    assert_eq!(
        asked[0].credentials,
        vec![shards_api::image::CredentialGrant {
            reference: "anthropic/ada".to_owned(),
            toward: vec!["api.anthropic.com".to_owned()],
            placeholder: PLACEHOLDER.to_owned(),
        }]
    );
    let lessor = asked[0].lessor.clone().expect("the boot's run lessor");
    let leased = lessor.0.lease("anthropic/ada", None).unwrap();
    assert_eq!(leased.value.0, b"sk-ant-api03-real");
    assert!(broker.holds(&leased.lease).unwrap());
}

/// The review's B-13 (AC-T7.4.1; T-T7.1.2): an image runtime opens its records as a node recovers its own. Do open an
/// image runtime and create an image VM; end it, and leave beside the VM's record what a crash leaves: a record write's
/// temporary file, a record torn in the runtime place in this boot of the host, and a node VM row an earlier process
/// left Ready, its lock held by none; then open the runtime again, as kiln's restart does, and list. Expect the
/// temporary file removed, the node VM row let go, and the torn record kept, its name never reused, and listed by its
/// id, Empty, beside the VM, never hidden. Then name an earlier boot in the host's boot marker, as a reboot leaves it,
/// and open the runtime again. Expect the torn record removed, its run having ended with that boot, and the VM listed.
#[test]
fn b13_an_image_runtime_recovers_its_records_as_a_node_does_and_lists_one_it_cant_read() {
    let dir = tempfile::tempdir().unwrap();
    let records = dir.path().join("records");
    let images = runtime(dir.path(), &Arc::new(Model::default()));
    images.create(image_box("kiln-kept")).unwrap();
    drop(images);
    let torn = BoxId::new("kiln-torn").unwrap();
    let place = crate::store::runtime_record(&records.join("run"), &torn);
    std::fs::write(&place, b"{\"spec\":").unwrap();
    let temporary = place.with_file_name(".kiln-cut.tmp");
    std::fs::write(&temporary, b"{\"spec\":").unwrap();
    let row = hex_core::record::NodeVmRecord {
        version: hex_core::record::RECORD_VERSION,
        run: hex_api::NodeRun::from_bytes([7; 16]),
        image: hex_api::sim::SIM_IMAGE.to_owned(),
        allocation: allocation(),
        phase: hex_api::NodeVmPhase::Ready,
        diagnostics: None,
        exit: None,
        last_resort: None,
        started: Some(1),
    };
    let rows = records.join("state").join(".node-vms");
    std::fs::create_dir_all(&rows).unwrap();
    std::fs::write(
        rows.join(format!("{}.json", "07".repeat(16))),
        row.encode().unwrap(),
    )
    .unwrap();
    let images = runtime(dir.path(), &Arc::new(Model::default()));
    assert!(!temporary.exists(), "a crash's temporary file left");
    assert_eq!(Books::all_node_vms(images.as_ref()).unwrap(), []);
    let listed = |images: &ImageRuntime| -> Vec<(String, BoxState)> {
        images
            .list()
            .unwrap()
            .into_iter()
            .map(|info| (info.id.as_str().to_owned(), info.state))
            .collect()
    };
    assert_eq!(
        listed(&images),
        [
            ("kiln-kept".to_owned(), BoxState::Created),
            ("kiln-torn".to_owned(), BoxState::Empty)
        ]
    );
    let reused = images.create(image_box("kiln-torn"));
    assert!(reused.is_err() && place.exists(), "{reused:?}");
    drop(images);
    std::fs::write(records.join("state").join("@boot"), b"an-earlier-boot\n").unwrap();
    let images = runtime(dir.path(), &Arc::new(Model::default()));
    assert!(!place.exists(), "a torn record of an earlier boot kept");
    assert_eq!(
        listed(&images),
        [("kiln-kept".to_owned(), BoxState::Created)]
    );
}

/// The review's B-8 (batch 1114d's reads2; AC-T7.4.1): an image VM's call reads its box's record by its id, never every
/// record its runtime keeps. Do make 100 image VMs and one more, start that one, then inspect it, ask it started and
/// delete it. Expect no record file read by those three calls.
#[test]
fn reads2_b8_an_image_vms_calls_read_no_record_file() {
    let dir = tempfile::tempdir().unwrap();
    let images = runtime(dir.path(), &Arc::new(Model::default()));
    for n in 0..100 {
        images.create(image_box(&format!("kiln-b8-{n}"))).unwrap();
    }
    let id = images.create(image_box("kiln-b8-one")).unwrap().id;
    images.start(&id).unwrap();
    let before = images.store.reads();
    assert_eq!(images.inspect(&id).unwrap().state, BoxState::Running);
    assert_eq!(images.started(&id).unwrap().state, BoxState::Running);
    images.delete(&id).unwrap();
    let read = images.store.reads() - before;
    assert_eq!(
        read, 0,
        "an inspect, a started and a delete read {read} record files"
    );
}

/// The review's B-14 (batch 1114d's reads2; AC-T7.4.1): one image box's transaction holds its own box's turn alone, so
/// the lifecycles of different boxes never wait on each other. Do take box a's transaction and, while it holds, look at
/// box b's turn. Expect b's turn free.
#[test]
fn reads2_b14_one_boxs_transaction_leaves_anothers_turn_free() {
    let dir = tempfile::tempdir().unwrap();
    let images = runtime(dir.path(), &Arc::new(Model::default()));
    let (a, b) = (
        BoxId::new("kiln-b14-a").unwrap(),
        BoxId::new("kiln-b14-b").unwrap(),
    );
    let mut b_free = None;
    Books::with_transaction(images.as_ref(), &a, &[], &mut |_| {
        b_free = Some(images.turn_free(&b));
        Ok(())
    })
    .unwrap();
    assert_eq!(
        b_free,
        Some(true),
        "box b's turn taken while box a's transaction held"
    );
}

/// A warm room refusing every claim (kiln plan `01a1dd2e313f72f69c6d93061143cc2e`, P3.1): no pool of it starts a host.
struct NoRoom;

impl WarmRoom for NoRoom {
    fn claim(&self, _: &shards_api::image::WarmHost) -> bool {
        false
    }

    fn give(&self, _: &shards_api::image::WarmHost, _: shards_api::image::Gave) {}
}

/// Kiln plan `01a1dd2e313f72f69c6d93061143cc2e`, P3.1 (CP-T7.2; HEX-IMG-9), kiln's call: do hold an image runtime made
/// as kiln makes it to the pools' case, then bind it a warm room and hold it to the case again. Expect: before its room
/// it keeps none, a keep and a let-go refused `Unconfigured` naming the warm room; once given one, it keeps, lists and
/// lets go of pools as its class does.
#[test]
fn p3_1_an_image_runtime_keeps_warm_pools_once_its_owner_binds_their_room() {
    let dir = tempfile::tempdir().unwrap();
    let model = Arc::new(Model::default());
    let runtime = runtime(dir.path(), &model);
    let case = hex_api::conformance::IMAGE_CASES
        .iter()
        .find(|case| case.id == "HEX-IMG-9")
        .unwrap();
    let pool = hex_api::WarmPool {
        image: hex_api::sim::SIM_IMAGE.to_owned(),
        size: allocation().vm_size().unwrap(),
    };
    assert_eq!(runtime.keep_pool(&pool), Err(Refusal::no_warm_room()));
    case.check(runtime.as_ref(), &hex_api::sim::SimFixture::ShardsImage)
        .unwrap();
    runtime.bind_room(Arc::new(NoRoom)).unwrap();
    case.check(runtime.as_ref(), &hex_api::sim::SimFixture::ShardsImage)
        .unwrap();
    runtime.keep_pool(&pool).unwrap();
    assert_eq!(
        runtime.pools(),
        Ok(vec![hex_api::WarmHeld {
            pool,
            warm: 0,
            starting: 0
        }])
    );
}

/// Kiln plan 01a1dd2e…, P6.2 (AC-T7.4.1; Shards' D281): a person sees an agent's volumes and clears one through the
/// runtime kiln holds, which asks its own `shards` binary in its own Shards home, as it starts its run hosts. Do list
/// the home's volumes and clear two through a runtime whose `shards` answers as `shards api-volumes` does, in that home
/// alone: the listing two volumes, an agent's and a run's anonymous one, the clearing of one a run holds refused, of
/// another done. Expect both listed whole, by name, bytes, bound, charge, time and kind; the held one refused `Volume {
/// InUse }` in Shards' words; the other cleared.
#[cfg(unix)]
#[test]
fn p6_2_a_homes_volumes_are_listed_and_cleared_through_its_own_shards() {
    use shards_api::volumes::{ClearCause, HomeVolume as Listed, cleared, listed};
    use std::os::unix::fs::PermissionsExt as _;

    let dir = tempfile::tempdir().unwrap();
    let home = shards_api::RuntimePaths::new(dir.path().join("cache"), dir.path().join("home"))
        .unwrap()
        .home()
        .to_owned();
    let volumes = vec![
        Listed {
            name: "kiln-agent-cache".to_owned(),
            bytes: Some(3 << 20),
            bound: Some(1 << 30),
            charge: Some((3 << 20) + (4 << 10)),
            created_ns: 1_794_712_180_000_000_000,
            anonymous: false,
        },
        Listed {
            name: "0f".repeat(32),
            bytes: None,
            bound: None,
            charge: None,
            created_ns: 0,
            anonymous: true,
        },
    ];
    let held = "volume kiln-agent-cache is held by a run that plugged it";
    // Each frame as printf(1) writes its bytes back.
    let printf = |frame: Vec<u8>| {
        let octal: String = frame.iter().map(|b| format!("\\{b:03o}")).collect();
        format!("printf '{octal}'")
    };
    let script = format!(
        "#!/bin/sh\n\
         [ \"$1\" = api-volumes ] && [ \"$SHARDS_HOME\" = '{}' ] || exit 9\n\
         case \"$2\" in\n\
         list) {} ;;\n\
         clear) [ \"$3\" = kiln-agent-cache ] && {} || {} ;;\n\
         esac\n",
        home.display(),
        printf(listed(&volumes).unwrap()),
        printf(cleared(&Err((ClearCause::InUse, held.to_owned()))).unwrap()),
        printf(cleared(&Ok(())).unwrap()),
    );
    std::fs::write(dir.path().join("shards"), script).unwrap();
    std::fs::set_permissions(
        dir.path().join("shards"),
        std::fs::Permissions::from_mode(0o755),
    )
    .unwrap();
    let images = runtime(dir.path(), &Arc::new(Model::default()));
    let seen: Vec<_> = images
        .volumes()
        .unwrap()
        .into_iter()
        .map(|v| {
            (
                v.name,
                v.bytes,
                v.bound,
                v.charge,
                v.created_ns,
                v.anonymous,
            )
        })
        .collect();
    let asked: Vec<_> = volumes
        .into_iter()
        .map(|v| {
            (
                v.name,
                v.bytes,
                v.bound,
                v.charge,
                v.created_ns,
                v.anonymous,
            )
        })
        .collect();
    assert_eq!(seen, asked);
    assert_eq!(
        images.clear_volume("kiln-agent-cache"),
        Err(Refusal::Volume {
            cause: hex_api::VolumeCause::InUse,
            why: held.to_owned()
        })
    );
    assert_eq!(images.clear_volume("kiln-agent-notes"), Ok(()));
}

/// Kiln plan step 35's `/runs/control` (AC-T7.4.1): kiln holds the image runtime in process and reads its runs'
/// agent-control links through it, so the runtime passes `link` to its class, never answering the trait's default, which
/// says a box isn't a Shards image box. Do: ask the link of a box it holds none of, of an image VM created and not
/// started, and of one running, whose model run tells no agent-control endpoint. Expect: `NoSuchBox`; `NotRunning(id,
/// "link")` before its start; and, once it runs, `Broken`, the class finding no endpoint its run told.
#[test]
fn s35_the_image_runtime_passes_a_boxs_link_to_its_class() {
    let dir = tempfile::tempdir().unwrap();
    let images = runtime(dir.path(), &Arc::new(Model::default()));
    let unknown = BoxId::new("kiln-s35-none").unwrap();
    assert!(matches!(images.link(&unknown), Err(Refusal::NoSuchBox(_))));
    let id = images.create(image_box("kiln-s35-link")).unwrap().id;
    assert_eq!(
        images.link(&id),
        Err(Refusal::NotRunning(id.clone(), "link"))
    );
    images.start(&id).unwrap();
    let running = images.link(&id);
    assert!(
        matches!(&running, Err(Refusal::Broken(why)) if why.contains("told no agent-control endpoint")),
        "{running:?}"
    );
}
