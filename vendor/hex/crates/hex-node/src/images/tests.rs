//! The image class alone as a runtime (joint plan step 12's item B), over a model of Shards' image boot whose runs hold
//! until their stop: what its boxes' records are, where they are kept, and what a restart of its caller finds.

use std::sync::Mutex;

use hex_api::{Allocation, Bounds, BoxState, ClassSpec, ShardsImage};
use shards_api::image::{
    Bound, Channel, ImageGuest, ImageRefusal, ImageSpec, MemberEnd, MemberInfo, MemberName, Ready,
    RunEnd, StartedFrom, Stats, StatsScope,
};

use super::*;

/// Shards' image boot in the model: each boot answered at once with a run that holds until its stop.
#[derive(Default)]
struct Model {
    asked: Mutex<Vec<ImageSpec>>,
}

impl Images for Model {
    fn backend(&self) -> Result<shards_api::Backend, ImageRefusal> {
        Ok(shards_api::Backend::Kvm)
    }

    fn boot(&self, spec: &ImageSpec, _: Duration) -> Result<Box<dyn ImageGuest>, ImageRefusal> {
        self.asked.lock().unwrap().push(spec.clone());
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
    fn published(&self) -> &[Bound] {
        &[]
    }
    fn control(&self) -> Option<&shards_api::image::ControlAt> {
        None
    }
    fn rate_period(&self) -> Duration {
        Duration::from_millis(100)
    }
    fn started_from(&self) -> StartedFrom {
        StartedFrom::Booted
    }
    fn take_output(&self, _: &MemberName, _: Channel) -> Result<std::io::PipeReader, ImageRefusal> {
        Err(unasked())
    }
    fn take_input(&self, _: &MemberName) -> Result<std::io::PipeWriter, ImageRefusal> {
        Err(unasked())
    }
    fn wait_member(&self, _: &MemberName, _: Duration) -> Result<Option<MemberEnd>, ImageRefusal> {
        Err(unasked())
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
    fn plug(
        &self,
        _: &MemberName,
        _: &Path,
        _: &str,
        _: bool,
        _: Duration,
    ) -> Result<(), ImageRefusal> {
        Err(unasked())
    }
    fn unplug(&self, _: &str, _: Duration) -> Result<(), ImageRefusal> {
        Err(unasked())
    }
    fn stop(&self) -> Result<(), ImageRefusal> {
        self.end.lock().unwrap().get_or_insert(RunEnd::Stopped);
        Ok(())
    }
    fn wait_for(&self, _: Duration) -> Result<Option<RunEnd>, ImageRefusal> {
        Ok(self.end.lock().unwrap().clone())
    }
    fn keep_diagnostics(&self, _: std::fs::File) -> Result<(), ImageRefusal> {
        Ok(())
    }
}

/// kiln's id for a reserved run at its largest: `kiln-r<epoch>-<seq>`, each a u64 in decimal (goal 54's change 5).
const RESERVED: &str = "kiln-r18446744073709551615-18446744073709551615";

fn allocation() -> Allocation {
    hex_api::sim::sim_vm_allocation().unwrap()
}

fn image_box(id: &str) -> BoxSpec {
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

/// Joint plan step 12's item B (goal 54's changes 4 and 5; AC-T7.4.1): the image class alone serves a node with no guest
/// of Hex's. Do open an image runtime over a records directory of its own; create a box of another class; create, start,
/// inspect and list an image VM named by kiln's largest reservation id; create, start and delete another; then end the
/// runtime and open another over the same records, as kiln's restart does, and list. Expect the runtime to serve the
/// Shards class alone, the other class refused; each VM booted once, the first shown Running and listed under its id
/// byte for byte, as its record in the runtime's own store names it; the second's record gone with its delete, every call
/// on it then NoSuchBox; and the restarted runtime to list the first under the same id, held, as an earlier process's run
/// is until its host's reboot proves it ended.
#[test]
fn w12_an_image_runtime_serves_the_image_class_alone_on_a_store_of_its_own() {
    let dir = tempfile::tempdir().unwrap();
    let model = Arc::new(Model::default());
    let images = runtime(dir.path(), &model);
    assert_eq!(images.classes(), vec![Class::Shards]);
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
    images.start(&gone).unwrap();
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
