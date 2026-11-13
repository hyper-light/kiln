//! HEX-GUEST-21 (native plan step 5, docs/design/shards-guest.md §4a): each image's own layers, staged as a
//! tarball root, hold exactly what containerd's unpack of the same layers holds. "Stage each image's layers,
//! walk the staged root with `hex selftest-box tree`, expect every entry as ctr's snapshot has it": its kind,
//! mode, owner, mtime, link text, hard-link group, xattrs and sha256. The stated differences, and no others:
//! - atime (the walker doesn't write it: reading a file moves it);
//! - the root directory itself (the walker doesn't list it: its owner and times are whoever made it);
//! - the mtime of directories mkparent made or made a child in (both sides stamp them when the unpack runs):
//!   a directory's mtime may differ only where both sides' are from their own unpack, at or after its start;
//! - security.selinux labels the host assigned, which the image's layers don't carry.
//!
//! The reference is `bench/native/guest-tree/reference.py`'s: each image's layer blobs byte-exact from
//! containerd's content store and its snapshot's walk (`<image>/layers.txt`, `layer<i>.tar.gz`,
//! `reference.tree`), with `RECORD.txt` saying how each was made. Given as:
//! - `HEX_GUEST21_REFERENCE`: that folder;
//! - `HEX_GUEST21_SINCE`: unix seconds no later than the reference's unpack began (its RECORD).
//!
//! Hex's side, one of:
//! - a real guest (`HEX_GUEST_KERNEL`, `HEX_GUEST_INITRD`, `SHARDS_BIN`, as HEX-GUEST-19): the layers are
//!   staged into the guest's layer store over its staging port, and a `process` box in the guest running
//!   the guest's own `hex` walks them through the overlay the node mounts for a box of that image
//!   (`/init selftest-box tree --layers`), so fidelity is proved through the guest's real overlayfs;
//! - on Linux, as root, `HEX_GUEST21_LOCAL=1`: staged by the relay in this process, walked through a real
//!   overlay mount by this build's `hex` (the check before a guest run).
//!
//! `HEX_GUEST21_OUT`, when set, gets each image's Hex-side walk (`<image>.tree`) for the record. Run:
//! `cargo test -p hex-cli --test guest_tree -- --nocapture` with those set; elsewhere it says SKIP and why.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    clippy::arithmetic_side_effects,
    clippy::disallowed_macros
)]

use std::collections::BTreeMap;
use std::io::Read;
use std::path::{Path, PathBuf};

use hex_api::BoxId;
use hex_client::guest::stage::{Item, Source};

/// One image of the reference: its name, its layers in order, and ctr's walk.
struct Image {
    name: String,
    layers: Vec<PathBuf>,
    reference: String,
}

fn images(dir: &Path) -> Vec<Image> {
    let mut all: Vec<Image> = std::fs::read_dir(dir)
        .unwrap()
        .map(|e| e.unwrap().path())
        .filter(|p| p.join("reference.tree").exists() && p.join("layers.txt").exists())
        .map(|p| Image {
            name: p.file_name().unwrap().to_str().unwrap().to_owned(),
            layers: std::fs::read_to_string(p.join("layers.txt"))
                .unwrap()
                .lines()
                .filter(|l| !l.is_empty())
                .map(|l| p.join(l))
                .collect(),
            reference: std::fs::read_to_string(p.join("reference.tree")).unwrap(),
        })
        .collect();
    all.sort_by(|a, b| a.name.cmp(&b.name));
    all
}

/// Whether any of `layers` carries a security.selinux xattr of its own (a PAX `SCHILY.xattr.` or
/// `LIBARCHIVE.xattr.` key): then the image's labels are compared, else the host's are left out.
fn carries_selinux(layers: &[PathBuf]) -> bool {
    layers.iter().any(|layer| {
        let mut bytes = Vec::new();
        flate2::read::GzDecoder::new(std::fs::File::open(layer).unwrap())
            .read_to_end(&mut bytes)
            .unwrap();
        bytes
            .windows(b"xattr.security.selinux".len())
            .any(|w| w == b"xattr.security.selinux")
    })
}

/// A manifest's entries by path: kind, mode, uid, gid, mtime, link, group, xattrs, sha256.
fn entries(text: &str) -> BTreeMap<String, Vec<String>> {
    text.lines()
        .map(|line| {
            let mut fields: Vec<String> = line.split('\t').map(str::to_owned).collect();
            let path = fields.remove(0);
            assert_eq!(fields.len(), 9, "a manifest line: {line}");
            (path, fields)
        })
        .collect()
}

/// `xattrs` without security.selinux, when the host assigned it.
fn without_selinux(xattrs: &str) -> String {
    let kept: Vec<&str> = xattrs
        .split(',')
        .filter(|x| *x != "-" && !x.starts_with("security.selinux="))
        .collect();
    if kept.is_empty() {
        "-".to_owned()
    } else {
        kept.join(",")
    }
}

/// The seconds of a manifest's `seconds.nanoseconds` mtime.
fn secs(mtime: &str) -> i64 {
    mtime.split('.').next().unwrap().parse().unwrap()
}

/// What the comparison found: every difference past the stated ones, and how often each stated one was
/// used (non-vacuity counters: a directory mtime both sides stamped, a host-assigned label left out).
#[derive(Debug, Default)]
struct Found {
    diffs: Vec<String>,
    stamped: usize,
    labels: usize,
}

/// Every difference between ctr's walk and Hex's, past the stated ones.
fn differences(reference: &str, hex: &str, selinux: bool, since_ref: i64, since_hex: i64) -> Found {
    let (want, got) = (entries(reference), entries(hex));
    let mut found = Found::default();
    let out = &mut found.diffs;
    for path in want
        .keys()
        .chain(got.keys().filter(|k| !want.contains_key(*k)))
    {
        let (Some(w), Some(g)) = (want.get(path), got.get(path)) else {
            out.push(format!(
                "{path}: only in {}",
                if want.contains_key(path) {
                    "ctr's"
                } else {
                    "Hex's"
                }
            ));
            continue;
        };
        const NAMES: [&str; 9] = [
            "kind", "mode", "uid", "gid", "mtime", "link", "group", "xattrs", "sha256",
        ];
        for (i, name) in NAMES.iter().enumerate() {
            let (mut a, mut b) = (w[i].clone(), g[i].clone());
            if *name == "xattrs" && !selinux {
                let (a2, b2) = (without_selinux(&a), without_selinux(&b));
                found.labels += usize::from(a2 != a) + usize::from(b2 != b);
                (a, b) = (a2, b2);
            }
            if *name == "mtime" && w[0] == "d" && secs(&a) >= since_ref && secs(&b) >= since_hex {
                found.stamped += 1;
                continue;
            }
            if a != b {
                out.push(format!("{path}: {name} ctr {a} Hex {b}"));
            }
        }
    }
    found
}

fn now() -> i64 {
    i64::try_from(
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs(),
    )
    .unwrap()
}

/// The image's layers, each named by its digest (its bytes' sha256), as the host's node stages them (§4a's
/// layer store).
fn layers(image: &Image) -> Vec<hex_client::guest::stage::Layer> {
    image
        .layers
        .iter()
        .map(|path| hex_client::guest::stage::Layer {
            digest: Some(
                <sha2::Sha256 as sha2::Digest>::digest(std::fs::read(path).unwrap()).into(),
            ),
            path: path.clone(),
        })
        .collect()
}

fn items(image: &Image) -> Vec<Item> {
    vec![Item {
        name: "root".to_owned(),
        source: Source::Layers(layers(image)),
    }]
}

/// Box `id`'s layers under `staged`, lowest first, `:`-joined, each its `layers/<i>` (a link to the store's
/// folder, or the box's own): what the walker mounts as the overlay the guest's node mounts for the box.
fn box_layers(staged: &str, id: &BoxId, image: &Image) -> String {
    (0..image.layers.len())
        .map(|i| hex_client::guest::stage::box_layer_path(staged, id, i))
        .collect::<Vec<_>>()
        .join(":")
}

/// Hex's side: stages and walks each image, returning its walk.
trait Side {
    fn walk(&self, id: &BoxId, image: &Image) -> String;
}

#[cfg(target_os = "linux")]
mod local {
    use std::path::PathBuf;

    use hex_api::{BoxId, Refusal};
    use hex_client::guest::{Link, stage};
    use hex_core::stream::files::LIMITS;

    use super::{Image, Side, box_layers, items};

    /// The relay in this process: each dial a socket pair whose far end it serves.
    struct InProcess(PathBuf);

    impl Link for InProcess {
        fn dial(&self, _port: u32) -> Result<hex_client::guest::DuplexOwner, Refusal> {
            let (near, far) = std::os::unix::net::UnixStream::pair()
                .map_err(|e| Refusal::os("pairing a staging socket", e))?;
            let staged = self.0.clone();
            std::thread::spawn(move || {
                hex_node::relay::files::serve(Box::new(far), &staged, &LIMITS)
            });
            hex_client::guest::own_duplex(near)
        }
    }

    pub struct Local {
        pub dir: tempfile::TempDir,
    }

    impl Side for Local {
        fn walk(&self, id: &BoxId, image: &Image) -> String {
            let link = hex_client::guest::own_link(InProcess(self.dir.path().to_owned())).unwrap();
            stage::stage(link.as_ref(), id, &items(image), &LIMITS)
                .unwrap_or_else(|why| panic!("{}: staging: {why}", image.name));
            let staged = self.dir.path().to_str().unwrap();
            let out = std::process::Command::new(env!("CARGO_BIN_EXE_hex"))
                .args(["selftest-box", "tree", "--layers"])
                .arg(box_layers(staged, id, image))
                .output()
                .unwrap();
            assert!(
                out.status.success(),
                "{}: walk: {}",
                image.name,
                String::from_utf8_lossy(&out.stderr)
            );
            stage::unstage(link.as_ref(), id).unwrap();
            String::from_utf8(out.stdout).unwrap()
        }
    }
}

/// A real guest: the layers staged over its staging port, walked by a `process` box in it.
struct Guest {
    link: hex_client::guest::LinkOwner,
    client: hex_client::guest::Client,
    _guest: std::sync::Arc<dyn shards_api::Guest>,
}

impl Side for Guest {
    fn walk(&self, id: &BoxId, image: &Image) -> String {
        use hex_api::{Bounds, BoxSpec, ClassSpec, Exit, Program, Runtime, Stdio};
        use hex_client::guest::stage;
        use hex_core::stream::files::LIMITS;
        stage::stage(self.link.as_ref(), id, &items(image), &LIMITS)
            .unwrap_or_else(|why| panic!("{}: staging: {why}", image.name));
        let walker = BoxId::new(&format!("{}-walk", id.as_str())).unwrap();
        let _ = self.client.delete(&walker);
        self.client
            .create(BoxSpec {
                route: None,
                allocation: None,
                devices: Vec::new(),
                id: walker.clone(),
                bounds: Bounds::default(),
                class: ClassSpec::Process,
                owner: None,
                band: 0,
            })
            .unwrap();
        let layers = box_layers(stage::STAGED, id, image);
        let (mut r, w) = std::io::pipe().unwrap();
        let reader = std::thread::spawn(move || {
            let mut text = String::new();
            r.read_to_string(&mut text).unwrap();
            text
        });
        let mut p = self
            .client
            .exec(
                &walker,
                Program::of(&["/init", "selftest-box", "tree", "--layers", &layers]),
                Stdio {
                    stdout: Some(w),
                    ..Stdio::null()
                },
            )
            .unwrap();
        // The walk's output ends when it does (the fact); its end after that is recorded on every run, the
        // distribution its bound is to come from (the lead's rule for learned waits). Until that record
        // exists, it waits within the suites' program-end patience, a program's end being its fact.
        let text = reader.join().unwrap();
        let after = std::time::Instant::now();
        let exit = hex_api::conformance::end_of(&mut p).unwrap();
        eprintln!(
            "{}: the walk ended {} us after its output did",
            image.name,
            after.elapsed().as_micros()
        );
        self.client.delete(&walker).unwrap();
        stage::unstage(self.link.as_ref(), id).unwrap();
        assert_eq!(exit, Exit::Code(0), "{}: the walk", image.name);
        text
    }
}

fn given(name: &str) -> Option<PathBuf> {
    std::env::var_os(name).map(PathBuf::from)
}

/// A guest's page: 4 KiB, both guest kernels' (`CONFIG_PAGE_SIZE_4KB`, scripts/guest/kernel-needs.config).
const GUEST_PAGE: u64 = 4096;

/// The room `layers` take staged in the guest: each regular file's bytes in whole pages, as its tmpfs charges
/// them (§4a: "an entry is charged its bytes in whole blocks").
fn footprint(layers: &[PathBuf]) -> u64 {
    layers
        .iter()
        .map(|l| {
            let mut archive = tar::Archive::new(flate2::read::GzDecoder::new(
                std::fs::File::open(l).unwrap(),
            ));
            archive
                .entries()
                .unwrap()
                .map(Result::unwrap)
                .filter(|e| e.header().entry_type().is_file())
                .map(|e| e.size().div_ceil(GUEST_PAGE) * GUEST_PAGE)
                .sum::<u64>()
        })
        .sum()
}

/// What a guest holds back from its staged tmpfs for each byte more of its memory, as a divisor. The kernel's
/// own pages take 64 bytes of `struct page` for each 4 KiB page, and a tmpfs of half the memory loses half of
/// that, 1/128. Measured on studio-2 (HVF, Shards' 6.18.48 aarch64, 2026-11-07, launchd
/// hex.ns5route.g21r.20261107T182252Z): 142 MiB more memory, 2,920 to 3,062 MiB, held back 1,196,032 bytes
/// more, 1/124.5, taken as 1/124.
const HELD_BACK_DIVISOR: u64 = 124;

/// A guest of Shards' least size (one vCPU, `min_memory_mib`) with room for the largest image staged in it.
/// Its staged folder is a tmpfs of half its memory (tmpfs(5)) less what the guest holds back, so the room is
/// measured, not assumed. Booted with twice that image's footprint more than the least, the guest reads its
/// staged folder's free bytes (`selftest-box room`). If short, it boots once more, sized by that shortfall:
/// each MiB more gives half a MiB of tmpfs less what the guest holds back for it ([`HELD_BACK_DIVISOR`]).
/// Still short is a measurement fault, said as one, not retried. (HEX-GUEST-21 at 6c5fb78 refused grafana's
/// stage, sized by half the memory: a 2,868 MiB guest had 1,429,372,928 bytes free for 1,497,251,840 bytes
/// of pages.)
fn guest(images: &[Image]) -> Option<Guest> {
    let (kernel, initrd, binary) = (
        given("HEX_GUEST_KERNEL")?,
        given("HEX_GUEST_INITRD")?,
        given("SHARDS_BIN")?,
    );
    let need = images
        .iter()
        .map(|i| footprint(&i.layers))
        .max()
        .unwrap_or(0);
    let least = shards_api::min_memory_mib(std::env::consts::ARCH);
    let first = (least + (2 * need).div_ceil(1 << 20)).next_multiple_of(2);
    let guest = boot_guest(&kernel, &initrd, &binary, first);
    let room = guest.room();
    eprintln!(
        "guest at {first} MiB: its staged folder's room {room} bytes, the largest image's pages {need}"
    );
    if room >= need {
        return Some(guest);
    }
    guest._guest.stop().unwrap();
    let per_mib = (1u64 << 20) / 2 - (1u64 << 20) / HELD_BACK_DIVISOR;
    let second = (first + (need - room).div_ceil(per_mib)).next_multiple_of(2);
    let guest = boot_guest(&kernel, &initrd, &binary, second);
    let resized = guest.room();
    eprintln!("guest at {second} MiB: its staged folder's room {resized} bytes");
    assert!(
        resized >= need,
        "a measurement fault: {second} MiB, sized by {first} MiB's shortfall of {} bytes at {per_mib} \
         bytes of room a MiB, gave {resized} bytes of room for {need}",
        need - room
    );
    Some(guest)
}

fn boot_guest(kernel: &Path, initrd: &Path, binary: &Path, memory_mib: u64) -> Guest {
    use shards_api::Guests;
    let config = hex_runtime::guest::GuestConfig {
        binary: binary.to_path_buf(),
        kernel: kernel.to_path_buf(),
        initrd: initrd.to_path_buf(),
        vcpus: 1,
        gpu: false,
        memory_mib,
        shares: Vec::new(),
        staged: hex_client::guest::stage::STAGED.to_owned(),
    };
    let guest: std::sync::Arc<dyn shards_api::Guest> = shards_api::Shards::new()
        .boot(&config.spec())
        .unwrap()
        .into();
    let link = hex_client::guest::own_link(hex_client::guest::shards::ShardsLink::new(
        guest.clone(),
        // The first dial's bound as the node gives it before any boot (`first_dial_bound`, decision 9).
        hex_runtime::guest::recorded_dial_bound().expect("this host class's boot-to-ready record"),
    ))
    .unwrap();
    let client = hex_client::guest::Client::connect(link.clone()).unwrap();
    Guest {
        link,
        client,
        _guest: guest,
    }
}

impl Guest {
    /// The bytes its staged folder's file system has free, read in the guest by a `process` box.
    fn room(&self) -> u64 {
        use hex_api::{Bounds, BoxSpec, ClassSpec, Exit, Program, Runtime, Stdio};
        let id = BoxId::new("g21-room").unwrap();
        let _ = self.client.delete(&id);
        self.client
            .create(BoxSpec {
                route: None,
                allocation: None,
                devices: Vec::new(),
                id: id.clone(),
                bounds: Bounds::default(),
                class: ClassSpec::Process,
                owner: None,
                band: 0,
            })
            .unwrap();
        let (mut r, w) = std::io::pipe().unwrap();
        let reader = std::thread::spawn(move || {
            let mut text = String::new();
            r.read_to_string(&mut text).unwrap();
            text
        });
        let mut p = self
            .client
            .exec(
                &id,
                Program::of(&[
                    "/init",
                    "selftest-box",
                    "room",
                    hex_client::guest::stage::STAGED,
                ]),
                Stdio {
                    stdout: Some(w),
                    ..Stdio::null()
                },
            )
            .unwrap();
        let text = reader.join().unwrap();
        let exit = hex_api::conformance::end_of(&mut p).unwrap();
        self.client.delete(&id).unwrap();
        assert_eq!(exit, Exit::Code(0), "the room read: {text}");
        text.trim()
            .strip_prefix("bytes=")
            .unwrap_or_else(|| panic!("the room read said {text:?}"))
            .parse()
            .unwrap()
    }
}

#[test]
fn each_images_staged_root_holds_what_containerds_unpack_holds() {
    let (Some(reference), Some(since)) = (
        given("HEX_GUEST21_REFERENCE"),
        std::env::var("HEX_GUEST21_SINCE").ok(),
    ) else {
        eprintln!("SKIP: no reference given (HEX_GUEST21_REFERENCE, HEX_GUEST21_SINCE unset)");
        return;
    };
    let since_ref: i64 = since.parse().unwrap();
    let since_hex = now();
    let side: Box<dyn Side> = if std::env::var_os("HEX_GUEST21_LOCAL").is_some() {
        #[cfg(target_os = "linux")]
        {
            Box::new(local::Local {
                dir: tempfile::tempdir().unwrap(),
            })
        }
        #[cfg(not(target_os = "linux"))]
        {
            eprintln!("SKIP: HEX_GUEST21_LOCAL stages in-process, on Linux alone");
            return;
        }
    } else {
        match guest(&images(&reference)) {
            Some(g) => Box::new(g),
            None => {
                eprintln!(
                    "SKIP: no Shards guest given (HEX_GUEST_KERNEL, HEX_GUEST_INITRD, SHARDS_BIN) \
                     and not HEX_GUEST21_LOCAL"
                );
                return;
            }
        }
    };
    let all = images(&reference);
    assert!(!all.is_empty(), "no images under {}", reference.display());
    let mut failed = Vec::new();
    for (i, image) in all.iter().enumerate() {
        let id = BoxId::new(&format!("g21-{i}")).unwrap();
        let hex = side.walk(&id, image);
        if let Some(out) = given("HEX_GUEST21_OUT") {
            std::fs::create_dir_all(&out).unwrap();
            std::fs::write(out.join(format!("{}.tree", image.name)), &hex).unwrap();
        }
        let selinux = carries_selinux(&image.layers);
        let found = differences(&image.reference, &hex, selinux, since_ref, since_hex);
        let diffs = found.diffs;
        eprintln!(
            "{}: {} entries in ctr's, {} in Hex's, {} differences past the stated ones; stated: {} \
             directory mtimes both sides stamped, {} host-assigned labels left out{}",
            image.name,
            image.reference.lines().count(),
            hex.lines().count(),
            diffs.len(),
            found.stamped,
            found.labels,
            if selinux {
                " (its layers carry labels: compared)"
            } else {
                ""
            }
        );
        for d in &diffs {
            eprintln!("  {d}");
        }
        if !diffs.is_empty() {
            failed.push(image.name.clone());
        }
    }
    assert!(
        failed.is_empty(),
        "images that differ from ctr's: {failed:?}"
    );
}

/// HEX-GUEST-21's comparison itself: two walks that differ in one file's mode, in a directory's mtime from
/// a header, in a host-assigned label and in a mkparent directory's stamp: expect the mode and the header
/// mtime named, the label and the stamp passed as stated.
#[test]
fn the_comparison_names_every_difference_but_the_stated_ones() {
    let line = |p: &str, k: &str, mode: &str, mtime: &str, x: &str| {
        format!("\"{p}\"\t{k}\t{mode}\t0\t0\t{mtime}\t-\t-\t{x}\t-\n")
    };
    let reference = [
        line("a", "f", "0644", "100.000000000", "security.selinux=00"),
        line("d", "d", "0755", "200.000000000", "-"),
        line("m", "d", "0755", "5000.000000000", "-"),
    ]
    .concat();
    let hex = [
        line("a", "f", "0600", "100.000000000", "-"),
        line("d", "d", "0755", "201.000000000", "-"),
        line("m", "d", "0755", "9000.000000000", "-"),
    ]
    .concat();
    let found = differences(&reference, &hex, false, 4000, 8000);
    assert_eq!((found.stamped, found.labels), (1, 1));
    assert_eq!(
        found.diffs,
        [
            "\"a\": mode ctr 0644 Hex 0600",
            "\"d\": mtime ctr 200.000000000 Hex 201.000000000"
        ]
    );
    // An image whose layers carry labels has them compared.
    assert_eq!(
        differences(&reference, &hex, true, 4000, 8000).diffs.len(),
        3
    );
}
