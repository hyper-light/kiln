//! Joint plan step 12, AC-T7.4.1: the Shards class on Agentfile images (every case of `hex_api::conformance::
//! IMAGE_CASES`, HEX-IMG-1 on) through a real `hex node` on Linux KVM, each box a microVM Shards' D231 boots from a pinned image
//! under shards-init (docs/design/shards-image-class.md), and the run that takes the class's first wait records:
//! `image_boot`, `image_stats` and `image_stop` (bench/native/guest/records.tsv).
//!
//! The node runs in the waits census (`HEX_WAITS_CENSUS`, `hex_node::census`), which arms no wait: there the image
//! waits measure while no record holds them (`hex_runtime::guest::ImageWaits::recorded`), and the census file keeps
//! each answer of `hex.image.boot`, `hex.image.stats` and `hex.image.stop`. At its end the test prints, for each,
//! its count, fastest and slowest (`IMAGE-RECORD`); and, as it goes, every answer the client's own waits record
//! (`IMAGE-STEP`, each call's answer as the client saw it) and each case's verdict and time (`IMAGE-CASE`).
//!
//! HEX-IMG-4 awaits Shards' guest time per vCPU (`Upstream::GuestCpuTimePerVcpu`), which no Shards revision gives
//! yet: it must fail naming that, and every other case pass. Once Shards gives it, HEX-IMG-4 passes and this test
//! says so, failing until its expectation goes.
//!
//! Explicit inputs only, each by its own variable; a missing one fails by name. Run alone, `--ignored`, inside a
//! cgroup delegated to this binary (`HEX_DELEGATED`), as native_firecracker_smoke is:
//! - `HEX_IMAGE`: the image, `NAME@sha256:...`, with a `/bin/sh` (HEX-IMG-3's command), pulled by its digest into
//!   the node's Shards home before the node starts;
//! - `SHARDS_BIN`: the `shards` binary, the node's and the pull's; `SHARDS_KERNEL`: the kernel the run hosts boot
//!   (D231's own, as `shards run` takes it), so no first boot downloads one;
//! - the node's Shards guest configuration, as `hex node` reads it: `HEX_GUEST_KERNEL`, `HEX_GUEST_INITRD`,
//!   `HEX_GUEST_CPUS`, `HEX_GUEST_MEMORY_MIB`, `HEX_GUEST_COST_FILE`, `HEX_GUEST_COST_SHA256` and
//!   `HEX_GUEST_COST_EVIDENCE`;
//! - `HEX_IMAGE_CPU_MILLI` and `HEX_IMAGE_MEMORY_BYTES`: the workload each VM is admitted for, which the guest
//!   cost sizes (`Allocation::vm_size`);
//! - `HEX_IMAGE_WORK`: a directory of this run's own, made here; `HEX_IMAGE_WAIT_MS`: the run's explicit deadline,
//!   for the node's start and the whole suite; `HEX_WAITS_CENSUS_MAX`: the census's most lines.
//!
//! Run: `cargo test --release --locked -p hex-cli --test image_real -- --ignored --nocapture` with those set.
#![cfg(target_os = "linux")]
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    clippy::arithmetic_side_effects,
    clippy::disallowed_macros
)]

use std::fs;
use std::path::PathBuf;
use std::sync::mpsc;
use std::time::{Duration, Instant};

use hex_api::conformance::{self, Act, Failure, Fixture};
use hex_api::{
    Allocation, Bounds, BoxId, BoxSpec, Class, ClassSpec, Program, Runtime, ShardsImage,
};
use hex_client::Client;

#[path = "image_node/mod.rs"]
mod image_node;
use image_node::{Node, allocation, input, number, own_cgroup, pull, records, step};

/// Shards image VMs of the pinned image, each admitted for the run's workload: running its members alone, or a
/// command for the act a case asks of its run.
struct ImageBoxes {
    image: String,
    allocation: Allocation,
}

/// The command a run's act is, in the image's own shell: the image cases ask a run to fail with a code (HEX-IMG-3)
/// and a member's program to say words (HEX-IMG-5, refused before it runs).
fn command(act: &Act) -> Vec<String> {
    let shell = |script: String| vec!["/bin/sh".to_owned(), "-c".to_owned(), script];
    match act {
        Act::Fail(code) => shell(format!("exit {code}")),
        Act::Say(words) => vec!["/bin/echo".to_owned(), (*words).to_owned()],
        other => panic!("the image cases ask no {other:?} of a run"),
    }
}

impl ImageBoxes {
    fn box_spec(&self, id: BoxId, bounds: Bounds, command: Option<Vec<String>>) -> BoxSpec {
        BoxSpec {
            id,
            bounds,
            class: ClassSpec::ShardsImage(Box::new(ShardsImage {
                command,
                ..ShardsImage::new(&self.image)
            })),
            owner: None,
            devices: Vec::new(),
            allocation: Some(self.allocation),
            route: None,
        }
    }
}

impl Fixture for ImageBoxes {
    fn spec(&self, id: BoxId, bounds: Bounds) -> BoxSpec {
        self.box_spec(id, bounds, None)
    }

    fn init(&self, id: BoxId, act: &Act) -> BoxSpec {
        let command = match act {
            // A run that waits is the image's members alone: it holds until its stop.
            Act::Wait => None,
            act => Some(command(act)),
        };
        self.box_spec(id, Bounds::default(), command)
    }

    fn program(&self, act: &Act) -> Program {
        let args = command(act);
        let args: Vec<&str> = args.iter().map(String::as_str).collect();
        Program::of(&args)
    }
}

#[test]
#[ignore = "native: KVM, a delegated cgroup, a Shards guest configuration and a pinned image (HEX_IMAGE)"]
fn the_shards_image_class_holds_its_contract_through_a_real_node() {
    assert!(
        std::env::var_os("HEX_DELEGATED").is_some(),
        "run inside a cgroup delegated to this binary (HEX_DELEGATED)"
    );
    let image = input("HEX_IMAGE");
    ShardsImage::new(&image).check().unwrap();
    let shards = PathBuf::from(input("SHARDS_BIN"));
    let kernel = PathBuf::from(input("SHARDS_KERNEL"));
    assert!(kernel.is_file(), "SHARDS_KERNEL {kernel:?} is no file");
    let wait = Duration::from_millis(number("HEX_IMAGE_WAIT_MS"));
    assert!(!wait.is_zero(), "HEX_IMAGE_WAIT_MS is 0");
    let work = PathBuf::from(input("HEX_IMAGE_WORK"));
    assert!(!work.exists(), "a run's own directory: {work:?} exists");
    fs::create_dir_all(work.join("state")).unwrap();
    // A VM's start boots it, which the client's learned bounds (an OCI box's records) would refuse first: every
    // wait here measures, and the run's deadline bounds the whole suite instead.
    hex_core::patience::measure_all();
    assert!(
        hex_core::patience::census(step),
        "a census sink was installed before this one"
    );
    // Admitted as the node's own cost sizes a VM, from the same configuration the node reads.
    let allocation = allocation();
    eprintln!("IMAGE each VM {:?}", allocation.vm_size().unwrap());
    let own = own_cgroup("hex-image-real");
    pull(&shards, &image, &work.join("state"));
    let node = Node::start(&work, &own, wait, "hex-image-node");
    let socket = node.socket.clone();
    let (done, verdicts) = mpsc::channel();
    std::thread::spawn(move || {
        let client = Client::connect(&socket).unwrap();
        assert!(
            client.classes().contains(&Class::Shards),
            "{:?}",
            client.classes()
        );
        let fixture = ImageBoxes { image, allocation };
        let begun = Instant::now();
        let failures = conformance::run_image(&client, &fixture);
        eprintln!(
            "IMAGE-CASE all {} in {:?}",
            conformance::IMAGE_CASES.len(),
            begun.elapsed()
        );
        let left = client.list().unwrap();
        let _ = done.send((failures, left));
    });
    let (failures, left) = verdicts
        .recv_timeout(wait)
        .unwrap_or_else(|e| panic!("the image cases did not end within {wait:?}: {e}"));
    drop(node);
    records(&work.join("census.txt"));
    for case in conformance::IMAGE_CASES {
        let verdict = failures
            .iter()
            .find(|f| f.case == case.id)
            .map_or("passed".to_owned(), |f| format!("failed: {}", f.why));
        eprintln!("IMAGE-CASE {} {verdict}", case.id);
    }
    assert!(left.is_empty(), "a box is left: {left:?}");
    // Guest time per vCPU awaits Shards: HEX-IMG-4 fails naming it, typed, and nothing else fails.
    let (owed, failed): (Vec<&Failure>, Vec<&Failure>) =
        failures.iter().partition(|f| f.case == "HEX-IMG-4");
    assert!(failed.is_empty(), "the image cases failed: {failed:#?}");
    assert!(
        owed.iter()
            .any(|f| f.why.contains("guest time per vCPU awaits")),
        "HEX-IMG-4 passed, or failed for another reason than the guest time Shards owes: {owed:#?}; once Shards \
         gives guest time per vCPU, this expectation goes"
    );
}
