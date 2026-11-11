//! amdgpu's sysfs as fixtures: the DRM class captured on adacachy2 (2026-11-09: `card1`, a Radeon 610M
//! at 0000:79:00.0 on amdgpu, beside `card2`, a GeForce at 0000:01:00.0 on nvidia, their connectors,
//! render nodes and the class's `version`; kernel 7.3.3-1-cachyos, in-tree amdgpu; libdrm 2.4.134's
//! table, which doesn't name the 610M), and the errors the kernel documents each read answering: a GPU
//! asleep, a GPU unplugged, a kernel without a counter, counters that aren't numbers (`AC-T7.3.1`).

use std::cell::{Cell, RefCell};
use std::fs;
use std::io;
use std::os::unix::fs::{FileExt, symlink};
use std::path::{Path, PathBuf};

use super::{Card, Found, Paths, open};
use crate::tests::hostile;
use crate::{Absent, Gpu, Identity, Load, PciAddress, Skipped, Unread, Vendor};

/// The head of libdrm 2.4.134's `amdgpu.ids` and two of its entries, as adacachy2 has them.
const IDS: &str = "# List of AMDGPU IDs\n#\n# Syntax:\n# device_id,\trevision_id,\tproduct_name        <-- \
                   single tab after comma\n\n1.0.0\n1114,\tC2,\tAMD Radeon 860M Graphics\n1586,\tC1,\tAMD \
                   Radeon 8060S Graphics\n";

/// A sysfs tree in a temporary folder, laid out as the kernel lays it out.
struct Sysfs {
    _dir: tempfile::TempDir,
    root: PathBuf,
    drm: PathBuf,
    ids: PathBuf,
    module_version: PathBuf,
    os_release: PathBuf,
}

impl Sysfs {
    /// The class with what isn't a card in it, a kernel release and libdrm's table.
    fn new() -> Sysfs {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().to_owned();
        let drm = root.join("class/drm");
        fs::create_dir_all(&drm).unwrap();
        for driver in ["amdgpu", "nvidia"] {
            fs::create_dir_all(root.join("bus/pci/drivers").join(driver)).unwrap();
        }
        fs::write(drm.join("version"), "drm 1.1.0 20060810\n").unwrap();
        for node in ["renderD128", "renderD129"] {
            fs::create_dir(drm.join(node)).unwrap();
        }
        let sysfs = Sysfs {
            ids: root.join("amdgpu.ids"),
            module_version: root.join("module/amdgpu/version"),
            os_release: root.join("osrelease"),
            _dir: dir,
            root,
            drm,
        };
        fs::write(&sysfs.ids, IDS).unwrap();
        fs::write(&sysfs.os_release, "7.3.3-1-cachyos\n").unwrap();
        sysfs
    }

    fn paths(&self) -> Paths<'_> {
        Paths {
            drm: &self.drm,
            ids: &self.ids,
            module_version: &self.module_version,
            os_release: &self.os_release,
        }
    }

    /// `card<n>`, linked to the PCI device `pci` bound to `driver`, holding `attributes`, and a
    /// connector of its own.
    fn card(&self, n: u32, pci: &str, driver: &str, attributes: &[(&str, &str)]) -> PathBuf {
        let device = self.root.join("devices/pci0000:00").join(pci);
        fs::create_dir_all(&device).unwrap();
        symlink(
            self.root.join("bus/pci/drivers").join(driver),
            device.join("driver"),
        )
        .unwrap();
        for (name, value) in attributes {
            fs::write(device.join(name), value).unwrap();
        }
        let card = self.drm.join(format!("card{n}"));
        fs::create_dir(&card).unwrap();
        symlink(&device, card.join("device")).unwrap();
        fs::create_dir(self.drm.join(format!("card{n}-DP-{n}"))).unwrap();
        device
    }

    /// adacachy2's `card1` (its Radeon 610M) and `card2` (its GeForce), as captured.
    fn adacachy2() -> (Sysfs, PathBuf) {
        let sysfs = Sysfs::new();
        let amd = sysfs.card(1, "0000:79:00.0", "amdgpu", &RADEON_610M);
        sysfs.card(
            2,
            "0000:01:00.0",
            "nvidia",
            &[
                ("vendor", "0x10de\n"),
                ("device", "0x2d04\n"),
                ("revision", "0xa1\n"),
            ],
        );
        (sysfs, amd)
    }
}

/// adacachy2's `card1/device` attributes Hex reads, as captured.
const RADEON_610M: [(&str, &str); 6] = [
    ("vendor", "0x1002\n"),
    ("device", "0x13c0\n"),
    ("revision", "0xe5\n"),
    ("gpu_busy_percent", "0\n"),
    ("mem_info_vram_total", "2147483648\n"),
    ("mem_info_vram_used", "16711680\n"),
];

fn listed(found: &Found) -> Vec<Gpu> {
    found.gpus.iter().map(|(gpu, _)| gpu.clone()).collect()
}

fn the_610m() -> Gpu {
    Gpu {
        vendor: Vendor::Amd,
        model: "1002:13c0 rev e5".to_owned(),
        memory_total_bytes: 2_147_483_648,
        identity: Identity::Pci(PciAddress::parse("0000:79:00.0").unwrap()),
        driver_version: "7.3.3-1-cachyos".to_owned(),
    }
}

/// AC-T7.3.1: do open adacachy2's captured DRM class; expect its Radeon 610M alone, named by its PCI
/// ids (libdrm's table doesn't know it), with its VRAM and the in-tree driver's version, the kernel's
/// release; the GeForce (NVML's), the connectors, render nodes and `version` passed over.
#[test]
fn adacachy2s_amd_gpu_is_listed_as_captured() {
    let (sysfs, _) = Sysfs::adacachy2();
    let found = open(&sysfs.paths()).unwrap();
    assert_eq!(listed(&found), vec![the_610m()]);
    assert!(found.skipped.is_empty());
}

/// AC-T7.3.1: do open a card libdrm's table names (jupiter's Radeon 8060S, 1586 rev c1) beside a
/// DKMS-built amdgpu; expect the table's name and the module's own version.
#[test]
fn the_tables_name_and_a_dkms_drivers_version() {
    let sysfs = Sysfs::new();
    let mut attributes = RADEON_610M;
    attributes[1] = ("device", "0x1586\n");
    attributes[2] = ("revision", "0xc1\n");
    sysfs.card(0, "0000:c5:00.0", "amdgpu", &attributes);
    fs::create_dir_all(sysfs.module_version.parent().unwrap()).unwrap();
    fs::write(&sysfs.module_version, "6.10.5\n").unwrap();
    let gpus = listed(&open(&sysfs.paths()).unwrap());
    assert_eq!(gpus[0].model, "AMD Radeon 8060S Graphics");
    assert_eq!(gpus[0].driver_version, "6.10.5");
}

/// AC-T7.3.1: do sample the captured card, change its counters as the kernel would, and sample again;
/// expect each sample to read what the counters say then, through the files opened once.
#[test]
fn a_sample_reads_the_counters_now() {
    let (sysfs, device) = Sysfs::adacachy2();
    let found = open(&sysfs.paths()).unwrap();
    let card = &found.gpus[0].1;
    assert_eq!(
        card.sample(),
        Load {
            busy_percent: Ok(0),
            memory_used_bytes: Ok(16_711_680)
        }
    );
    fs::write(device.join("gpu_busy_percent"), "100\n").unwrap();
    fs::write(device.join("mem_info_vram_used"), "2147483648\n").unwrap();
    assert_eq!(
        card.sample(),
        Load {
            busy_percent: Ok(100),
            memory_used_bytes: Ok(2_147_483_648)
        }
    );
}

/// AC-T7.3.1: do open cards numbered against their bus order; expect them in PCI order.
#[test]
fn cards_are_listed_in_pci_order() {
    let sysfs = Sysfs::new();
    sysfs.card(0, "0000:c1:00.0", "amdgpu", &RADEON_610M);
    sysfs.card(1, "0000:03:00.0", "amdgpu", &RADEON_610M);
    let order: Vec<String> = listed(&open(&sysfs.paths()).unwrap())
        .iter()
        .map(|g| g.pci().unwrap().to_string())
        .collect();
    assert_eq!(order, ["0000:03:00.0", "0000:c1:00.0"]);
}

/// AC-T7.3.1, CLAUDE.md §3: do open an amdgpu card whose kernel has no `gpu_busy_percent`, beside one
/// that has; expect the first skipped as unsupported and the second listed.
#[test]
fn a_card_without_a_counter_is_skipped_with_why() {
    let (sysfs, _) = Sysfs::adacachy2();
    let without: Vec<_> = RADEON_610M
        .into_iter()
        .filter(|(name, _)| *name != "gpu_busy_percent")
        .collect();
    sysfs.card(3, "0000:c1:00.0", "amdgpu", &without);
    let found = open(&sysfs.paths()).unwrap();
    assert_eq!(listed(&found), vec![the_610m()]);
    assert_eq!(
        found.skipped,
        vec![Skipped::Drm {
            card: 3,
            why: Unread::Unsupported
        }]
    );
}

/// AC-T7.3.1, hot-unplug: do unplug the card (its device and its `card1` gone from the class) and open
/// again; expect nothing listed, and a node without the DRM class refused with the OS's error.
#[test]
fn after_an_unplug_open_lists_what_is_there() {
    let (sysfs, device) = Sysfs::adacachy2();
    fs::remove_dir_all(sysfs.drm.join("card1")).unwrap();
    fs::remove_dir_all(device).unwrap();
    let found = open(&sysfs.paths()).unwrap();
    assert!(found.gpus.is_empty() && found.skipped.is_empty());
    fs::remove_dir_all(&sysfs.drm).unwrap();
    assert_eq!(
        open(&sysfs.paths()).err(),
        Some(Absent::Os {
            errno: libc::ENOENT
        })
    );
}

/// A counter file answering from a recording: a line, or an OS error number, counting its reads.
struct Counter {
    answer: RefCell<Result<Vec<u8>, i32>>,
    reads: Cell<u32>,
}

impl Counter {
    fn says(line: &[u8]) -> Counter {
        Counter {
            answer: RefCell::new(Ok(line.to_vec())),
            reads: Cell::new(0),
        }
    }
}

impl FileExt for Counter {
    fn read_at(&self, buf: &mut [u8], offset: u64) -> io::Result<usize> {
        assert_eq!(offset, 0, "a counter is read from its start");
        self.reads.set(self.reads.get() + 1);
        match &*self.answer.borrow() {
            Ok(line) => {
                let n = line.len().min(buf.len());
                buf[..n].copy_from_slice(&line[..n]);
                Ok(n)
            }
            Err(errno) => Err(io::Error::from_raw_os_error(*errno)),
        }
    }

    fn write_at(&self, _: &[u8], _: u64) -> io::Result<usize> {
        Err(io::Error::from(io::ErrorKind::Unsupported))
    }
}

fn card(busy: &[u8], vram_used: &[u8]) -> Card<Counter> {
    Card {
        busy: Counter::says(busy),
        vram_used: Counter::says(vram_used),
    }
}

/// AC-T7.3.1, hot-unplug: do sample a card, then remove its device, after which the kernel answers
/// its open counters `ENODEV` (fs/kernfs/file.c); expect both `Gone`, for one read, not two.
#[test]
fn a_card_unplugged_between_samples_reads_gone() {
    let card = card(b"37\n", b"16711680\n");
    assert_eq!(
        card.sample(),
        Load {
            busy_percent: Ok(37),
            memory_used_bytes: Ok(16_711_680)
        }
    );
    *card.busy.answer.borrow_mut() = Err(libc::ENODEV);
    *card.vram_used.answer.borrow_mut() = Err(libc::ENODEV);
    assert_eq!(
        card.sample(),
        Load {
            busy_percent: Err(Unread::Gone),
            memory_used_bytes: Err(Unread::Gone)
        }
    );
    assert_eq!((card.busy.reads.get(), card.vram_used.reads.get()), (2, 1));
}

/// AC-T7.3.1: do sample a runtime-suspended GPU, whose busy counter amdgpu answers `EPERM` rather
/// than wake it (amdgpu_pm.c); expect it `Asleep`, and its VRAM, which needs no wake, read.
#[test]
fn an_asleep_gpu_reads_asleep_and_its_memory() {
    let card = card(b"", b"16711680\n");
    *card.busy.answer.borrow_mut() = Err(libc::EPERM);
    assert_eq!(
        card.sample(),
        Load {
            busy_percent: Err(Unread::Asleep),
            memory_used_bytes: Ok(16_711_680)
        }
    );
    *card.busy.answer.borrow_mut() = Err(libc::EBUSY);
    assert_eq!(
        card.sample().busy_percent,
        Err(Unread::Os { errno: libc::EBUSY })
    );
}

/// AC-T7.3.1, CLAUDE.md §3: do read counters that aren't what `sysfs_emit` prints (a percent past
/// 100, a sign, a space, no newline, a value past u64, a line past the counters' longest); expect each
/// refused as malformed, never clamped or cut.
#[test]
fn counters_that_arent_numbers_are_malformed() {
    for busy in [
        &b"101\n"[..],
        b"-1\n",
        b" 5\n",
        b"5",
        b"\n",
        b"",
        b"4294967295\n",
        b"5\n5\n",
    ] {
        assert_eq!(
            card(busy, b"0\n").sample().busy_percent,
            Err(Unread::Malformed),
            "{busy:?}"
        );
    }
    for vram in [
        &b"18446744073709551616\n"[..],
        b"184467440737095516150\n",
        b"0x10\n",
        b"1e9\n",
    ] {
        assert_eq!(
            card(b"0\n", vram).sample().memory_used_bytes,
            Err(Unread::Malformed),
            "{vram:?}"
        );
    }
    assert_eq!(
        card(b"100\n", b"18446744073709551615\n").sample(),
        Load {
            busy_percent: Ok(100),
            memory_used_bytes: Ok(u64::MAX)
        }
    );
}

/// AC-T7.3.1, hostile input: do read 100,000 counters of digits and noise; expect no panic, and a
/// value exactly when the line is digits and its newline.
#[test]
fn hostile_counters_never_panic() {
    let mut x = 0x2545_f491_4f6c_dd1d;
    let mut read = 0;
    for _ in 0..100_000 {
        let line = hostile(&mut x, b"0123456789\n\n -+x\0", 24);
        let expected = match line.split_last() {
            Some((b'\n', digits))
                if !digits.is_empty() && digits.iter().all(u8::is_ascii_digit) =>
            {
                std::str::from_utf8(digits).unwrap().parse::<u64>().ok()
            }
            _ => None,
        };
        let got = card(b"0\n", &line).sample().memory_used_bytes.ok();
        assert_eq!(got, expected.filter(|_| line.len() <= 21), "{line:?}");
        read += u32::from(got.is_some());
    }
    assert!(read > 0);
}

/// AC-T7.3.1, hostile input: do look cards up in tables of noise, overlong lines and names past
/// NVML's 95 bytes; expect no panic, and only a well-formed line of the card's ids to name it.
#[test]
fn hostile_tables_never_panic_or_misname() {
    let sysfs = Sysfs::new();
    let long = "A".repeat(96);
    let table = format!(
        "{}\n13C0,\tE5,\t{long}\n13C0,\tE5\n13C0,E5,\n{}13C0,\tE5,\tAMD Radeon 610M\n",
        "x".repeat(100_000),
        "13C0,\tE6,\tnot this one\n"
    );
    fs::write(&sysfs.ids, table).unwrap();
    assert_eq!(super::model(&sysfs.ids, 0x13c0, 0xe5), "AMD Radeon 610M");
    let mut x = 0x9e37_79b9_7f4a_7c15;
    for _ in 0..2_000 {
        let noise = hostile(&mut x, b"13C0E5,\t\n# AMD", 300);
        fs::write(&sysfs.ids, &noise).unwrap();
        let model = super::model(&sysfs.ids, 0x13c0, 0xe5);
        assert!(model == "1002:13c0 rev e5" || noise.windows(4).any(|w| w == b"13C0"));
    }
    fs::remove_file(&sysfs.ids).unwrap();
    assert_eq!(
        super::model(Path::new(&sysfs.ids), 0x13c0, 0xe5),
        "1002:13c0 rev e5"
    );
}
