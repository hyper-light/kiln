//! HEX-GUEST-39's measure (native plan step 5): what an upgrade's first create costs a user. One process makes
//! the node (`WithGuest`, its `oci` class a real Shards guest) and creates and deletes one `oci` box, timing
//! the node's making and the create. Run once per simulated upgrade (a fresh copy of Shards and its VM
//! process under new signing identifiers, and a fresh `SHARDS_CACHE`: bench/native/guest/first-create), so
//! the create is the first of a new build. The same file builds against a tree whose node boots its guest
//! at the first create and one whose node boots it when made.
//!
//! Given as (else it says SKIP and why): `HEX_GUEST_KERNEL`, `HEX_GUEST_INITRD`, `SHARDS_BIN`, as HEX-GUEST-19;
//! `HEX_FIRST_IMAGE`, an image folder whose `blobs.txt` names its layers lowest first (bench/native/guest-tree's
//! layout). Prints `FIRST make_ms=<n> create_ms=<n> delete_ms=<n>`. Run:
//! `cargo test --release -p hex-cli --test guest_first_create -- --nocapture`.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    clippy::arithmetic_side_effects,
    clippy::disallowed_macros
)]

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Instant;

use hex_api::{
    Bounds, BoxId, BoxSpec, ClassSpec, Network, OciSpec, Program, Refusal, Rootfs, Runtime,
};
use hex_runtime::guest::{GuestConfig, GuestRuntime, WithGuest};

/// A node as either tree's `WithGuest::new` gives it: the node, or the node or its refusal.
trait Made<T> {
    fn made(self) -> T;
}

impl<T> Made<T> for T {
    fn made(self) -> T {
        self
    }
}

impl<T> Made<T> for Result<T, Refusal> {
    fn made(self) -> T {
        self.unwrap_or_else(|why| panic!("making the node: {why}"))
    }
}

fn given(name: &str) -> Option<PathBuf> {
    std::env::var_os(name).map(PathBuf::from)
}

#[test]
fn an_upgrades_first_create_is_timed() {
    let (Some(kernel), Some(initrd), Some(binary), Some(image)) = (
        given("HEX_GUEST_KERNEL"),
        given("HEX_GUEST_INITRD"),
        given("SHARDS_BIN"),
        given("HEX_FIRST_IMAGE"),
    ) else {
        eprintln!(
            "SKIP: no guest or image given (HEX_GUEST_KERNEL, HEX_GUEST_INITRD, SHARDS_BIN, HEX_FIRST_IMAGE)"
        );
        return;
    };
    let layers: Vec<PathBuf> = std::fs::read_to_string(image.join("blobs.txt"))
        .unwrap()
        .lines()
        .map(|l| image.join(l))
        .collect();
    let config = GuestConfig {
        binary,
        kernel,
        initrd,
        vcpus: 1,
        gpu: false,
        // Shards' least and twice HEX-GUEST-19's measured peak, as guest_real sizes it.
        memory_mib: shards_api::min_memory_mib(std::env::consts::ARCH) + 2 * 284,
        shares: Vec::new(),
        staged: hex_client::guest::stage::STAGED.to_owned(),
    };
    let making = Instant::now();
    let node: WithGuest<hex_api::sim::SimRuntime> = WithGuest::new(
        hex_api::sim::SimRuntime::new(),
        GuestRuntime::new(Arc::new(shards_api::Shards::new()), config),
    )
    .made();
    let made = making.elapsed().as_secs_f64() * 1e3;
    let id = BoxId::new("first").unwrap();
    let creating = Instant::now();
    node.create(BoxSpec {
        route: None,
        allocation: None,
        devices: Vec::new(),
        id: id.clone(),
        bounds: Bounds::default(),
        class: ClassSpec::Oci(Box::new(OciSpec {
            rootfs: Rootfs::Layers(layers),
            init: Program::of(&["/bin/true"]),
            mounts: Vec::new(),
            hostname: None,
            network: Network::Private,
            hosts: Vec::new(),
        })),
        owner: None,
        band: 0,
    })
    .unwrap();
    let created = creating.elapsed().as_secs_f64() * 1e3;
    let deleting = Instant::now();
    node.delete(&id).unwrap();
    let deleted = deleting.elapsed().as_secs_f64() * 1e3;
    println!("FIRST make_ms={made:.1} create_ms={created:.1} delete_ms={deleted:.1}");
}
