//! The image class's record writes under the recorded node (the HVF run's HEX-IMG-4, 2026-11-14; decision 9;
//! AC-T7.4.1): the real Recorded, its Store and the image class, over the image runtime tests' model of Shards' image
//! boot, whose runs hold until their stop.

use super::*;
use crate::images::tests::{Model, image_box};
use hex_api::sim::SimRuntime;
use hex_api::{ClassSpec, Exit};
use hex_core::patience::Patience;
use hex_runtime::guest::{GuestConfig, GuestRuntime, Side, WithGuest};
use hex_testkit::waits::follow::{self, Party};

fn route(_: &SimRuntime, class: &ClassSpec) -> Result<Side, Refusal> {
    Ok(if class.class() == Class::Process {
        Side::Host
    } else {
        Side::Guest
    })
}

/// A recorded node whose guest runtime serves image VMs as `hex node` configures it (hex-cli's configured_guest), its
/// image boot `model`, everything under `root`.
fn image_node(root: &Path, model: &Arc<Model>) -> Arc<Recorded<WithGuest<SimRuntime>>> {
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
    let guest = GuestRuntime::new(
        Arc::new(shards_api::sim::SimGuests::new(|_| {
            shards_api::End::PowerOff
        })),
        config,
    )
    .recorded_at(root.join("guest"))
    .with_runtime_paths(
        shards_api::RuntimePaths::new(root.join("cache"), root.join("home")).unwrap(),
    )
    .with_images(
        Ok(Arc::clone(model) as Arc<dyn shards_api::image::Images>),
        Ok(hex_runtime::guest::ImageWaits::new()),
        root.join("image-diagnostics"),
    );
    let node = Arc::new(
        Recorded::open(
            WithGuest::configured(SimRuntime::new(), guest, route),
            &root.join("state"),
        )
        .unwrap(),
    );
    node.bind_guest_books().unwrap();
    node
}

/// The KILL waiting for the box's turn under way: this test's fact alone (hex_testkit::waits).
static IMG4_KILL_WAITS: Patience = Patience::measuring("test.hex_node.img4_kill_waits");

/// HEX-IMG-4 (the HVF run of 2026-11-14 at cf0ca967; decision 9; AC-T7.4.1): a user's KILL of an image VM is never
/// refused Busy because another call's write of the box's record is under way, as the charge learner's is where its ask
/// settles the same end: a record write makes no native call, so it waits for the box's turn, following its holder.
/// Do: start an image VM through the recorded node; take the box's turn, as another call's record write holds it; KILL
/// the VM; once the KILL waits for the turn, let it go. Expect: the KILL answers Ok, and the box's record has its end,
/// Signal(9).
#[test]
fn img4_an_image_vms_kill_whose_end_meets_another_calls_turn_waits_for_it_never_busy() {
    let dir = tempfile::tempdir().unwrap();
    let node = image_node(dir.path(), &Arc::new(Model::default()));
    let id = node.create(image_box("img4-kill")).unwrap().id;
    node.start(&id).unwrap();
    let held = node.books.lifecycle.try_take(&id).unwrap();
    let waited = node.books.lifecycle.waited();
    let killed = std::thread::scope(|scope| {
        let killing = scope.spawn(|| node.signal(&id, Signal::KILL));
        follow::until(
            &IMG4_KILL_WAITS,
            "the KILL waiting for the box's turn",
            || {
                if killing.is_finished() {
                    Party::Ended
                } else {
                    Party::Live
                }
            },
            || killing.is_finished() || node.books.lifecycle.waited() > waited,
        )
        .unwrap();
        drop(held);
        killing.join().unwrap()
    });
    assert_eq!(killed, Ok(()));
    let record = node.books.store.read_lifecycle(&id).unwrap().unwrap();
    assert_eq!(record.exit, Some(Exit::Signal(Signal::KILL.number())));
}
