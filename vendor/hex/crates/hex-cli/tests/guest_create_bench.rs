//! HEX-GUEST-22 (native plan step 5, docs/design/shards-guest.md §4a): what a box's life costs through a real
//! Shards guest, create and delete, the record the guest's layer store is measured by (the store moves cost
//! between them: whole-root staging removed a box's root at its delete). Each round creates and deletes an
//! `oci` box of each image in turn (interleaved), through `GuestRuntime` from this host, and times each create,
//! its delete, and the two together: the first of an image after the guest's boot, and every later one. It
//! prints each, and, per image, p50 / p99 / max of the later ones.
//!
//! Given as (else it says SKIP and why):
//! - a guest: `HEX_GUEST_KERNEL`, `HEX_GUEST_INITRD`, `SHARDS_BIN`, as HEX-GUEST-19;
//! - `HEX_GUEST22_IMAGES`: a folder of images, each `<name>/<list>` naming its layer tarballs lowest first,
//!   relative to `<name>` (bench/native/guest-tree's layout); `HEX_GUEST22_LIST` names the list, `layers.txt`
//!   (the image's own `layer<i>.tar.gz`) unless set (`blobs.txt`: the same files linked as `blobs/sha256/<hex>`);
//! - `HEX_GUEST22_ROUNDS`: how many rounds, the record's N;
//! - `HEX_GUEST22_MEMX`: the guest's memory past Shards' least, in multiples of the images' layers unpacked
//!   (2 unless set: its tmpfs, half its memory, holds them once);
//! - `HEX_GUEST22_TURN`, optional: a folder holding FIFOs `go` and `done`. Before each round the bench reads
//!   a byte from `go`, and after it writes one to `done`, so a driver can interleave two arms' benches round
//!   by round, whatever load comes hitting both alike (bench/native/guest/create-life/pair.sh).
//!
//! The file needs nothing of the store's own API, so the same file measures whole-root staging's commit and
//! the store's. Run: `cargo test --release -p hex-cli --test guest_create_bench -- --nocapture` with those set.

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

use hex_api::{Bounds, BoxId, BoxSpec, ClassSpec, Network, OciSpec, Program, Rootfs, Runtime};
use hex_runtime::guest::{GuestConfig, GuestRuntime};

fn given(name: &str) -> Option<PathBuf> {
    std::env::var_os(name).map(PathBuf::from)
}

/// The value at the `percent`th percentile of `sorted`, by nearest rank.
fn at(sorted: &[f64], percent: usize) -> f64 {
    let rank = (sorted.len() * percent)
        .div_ceil(100)
        .clamp(1, sorted.len());
    sorted[rank - 1]
}

#[test]
fn a_multi_layer_create_through_a_real_guest_is_timed() {
    let (Some(kernel), Some(initrd), Some(binary), Some(images), Some(rounds)) = (
        given("HEX_GUEST_KERNEL"),
        given("HEX_GUEST_INITRD"),
        given("SHARDS_BIN"),
        given("HEX_GUEST22_IMAGES"),
        std::env::var("HEX_GUEST22_ROUNDS").ok(),
    ) else {
        eprintln!(
            "SKIP: no guest or images given (HEX_GUEST_KERNEL, HEX_GUEST_INITRD, SHARDS_BIN, \
             HEX_GUEST22_IMAGES, HEX_GUEST22_ROUNDS)"
        );
        return;
    };
    let rounds: usize = rounds.parse().unwrap();
    let list = std::env::var("HEX_GUEST22_LIST").unwrap_or_else(|_| "layers.txt".to_owned());
    let memx: u64 = std::env::var("HEX_GUEST22_MEMX").map_or(2, |m| m.parse().unwrap());
    let mut names: Vec<String> = std::fs::read_dir(&images)
        .unwrap()
        .map(|e| e.unwrap().file_name().into_string().unwrap())
        .filter(|n| images.join(n).join(&list).exists())
        .collect();
    names.sort();
    let layers: Vec<Vec<PathBuf>> = names
        .iter()
        .map(|n| {
            std::fs::read_to_string(images.join(n).join(&list))
                .unwrap()
                .lines()
                .filter(|l| !l.is_empty())
                .map(|l| images.join(n).join(l))
                .collect()
        })
        .collect();
    // The guest's tmpfs holds half its memory (tmpfs(5)): MEMX times the images' layers, unpacked, and
    // Shards' least besides.
    let unpacked: u64 = layers
        .iter()
        .flatten()
        .map(|l| {
            std::io::copy(
                &mut flate2::read::GzDecoder::new(std::fs::File::open(l).unwrap()),
                &mut std::io::sink(),
            )
            .unwrap()
        })
        .sum();
    let config = GuestConfig {
        binary,
        kernel,
        initrd,
        vcpus: 1,
        gpu: false,
        memory_mib: (shards_api::min_memory_mib(std::env::consts::ARCH)
            + (memx * unpacked).div_ceil(1 << 20))
        .next_multiple_of(2),
        shares: Vec::new(),
        staged: hex_client::guest::stage::STAGED.to_owned(),
    };
    eprintln!(
        "images {names:?} by {list}, {rounds} rounds, guest memory {} MiB (MEMX {memx})",
        config.memory_mib
    );
    let node = GuestRuntime::new(Arc::new(shards_api::Shards::new()), config);
    // Per image, each round's create, delete, and the two together, in ms.
    let mut times: Vec<[Vec<f64>; 3]> = vec![Default::default(); names.len()];
    let turn = given("HEX_GUEST22_TURN");
    for round in 0..rounds {
        if let Some(turn) = &turn {
            let mut byte = [0u8; 1];
            std::io::Read::read_exact(
                &mut std::fs::File::open(turn.join("go")).unwrap(),
                &mut byte,
            )
            .unwrap();
        }
        for (i, name) in names.iter().enumerate() {
            let id = BoxId::new(&format!("b{round}-{i}")).unwrap();
            let spec = BoxSpec {
                route: None,
                allocation: None,
                devices: Vec::new(),
                id: id.clone(),
                bounds: Bounds::default(),
                class: ClassSpec::Oci(Box::new(OciSpec {
                    rootfs: Rootfs::Layers(layers[i].clone()),
                    init: Program::of(&["/bin/true"]),
                    mounts: Vec::new(),
                    hostname: None,
                    network: Network::Private,
                    hosts: Vec::new(),
                })),
                owner: None,
            };
            let began = Instant::now();
            node.create(spec)
                .unwrap_or_else(|why| panic!("{name}: create: {why}"));
            let created = began.elapsed().as_secs_f64() * 1e3;
            let deleting = Instant::now();
            node.delete(&id).unwrap();
            let deleted = deleting.elapsed().as_secs_f64() * 1e3;
            eprintln!(
                "round {round} {name}: create {created:.1} ms delete {deleted:.1} ms life {:.1} ms",
                created + deleted
            );
            for (k, ms) in [created, deleted, created + deleted]
                .into_iter()
                .enumerate()
            {
                times[i][k].push(ms);
            }
        }
        if let Some(turn) = &turn {
            std::io::Write::write_all(
                &mut std::fs::OpenOptions::new()
                    .write(true)
                    .open(turn.join("done"))
                    .unwrap(),
                b".",
            )
            .unwrap();
        }
    }
    for (i, name) in names.iter().enumerate() {
        for (k, what) in ["create", "delete", "life"].iter().enumerate() {
            let first = times[i][k][0];
            let mut later = times[i][k][1..].to_vec();
            later.sort_by(f64::total_cmp);
            if later.is_empty() {
                eprintln!("{name} {what}: first {first:.1} ms");
                continue;
            }
            eprintln!(
                "{name} {what}: first {first:.1} ms; later (N={}) p50 {:.1} p99 {:.1} max {:.1} ms",
                later.len(),
                at(&later, 50),
                at(&later, 99),
                later[later.len() - 1]
            );
        }
    }
}
