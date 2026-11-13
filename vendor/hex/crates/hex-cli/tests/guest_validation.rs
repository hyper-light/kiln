//! HEX-GUEST-20 (native plan step 5, docs/design/shards-guest.md §6a): the OCI runtime-spec validation
//! (opencontainers/runtime-tools bd3b0bd) in a Shards guest, reached from the host's node through the seam.
//! The host's node (`hex_runtime::guest`, over `shards_api::Shards`) boots the guest and makes the lab: an
//! `oci` box from a bundle whose root is an Alpine userland (bench/native/oci-validation/lab.sh), privileged as
//! the Linux run's container is (bench/oci-validation/run_hex.sh), running the unchanged
//! bench/oci-validation/inside.sh against the guest's `hex oci`. The runtime-tools checkout (read-only) and an
//! out directory (read-write) are the run's two shares. "Run the suite in the guest, expect every test that
//! passes on Linux (bench/oci-validation/results-hex-linux.tsv) to pass, and the guest booted once."
//!
//! It needs a guest and the lab, so it runs only where they're given, and says SKIP and why elsewhere:
//! - `HEX_GUEST_KERNEL`, `HEX_GUEST_INITRD`, `SHARDS_BIN`: the guest, as HEX-GUEST-19's;
//! - `HEX_GUEST_LINUX_HEX`: the guest's static `hex`, put at the lab's /opt/hex;
//! - `HEX_GUEST_LAB`: the lab's root (lab.sh's output for the guest's arch), a directory or, on any host and
//!   on Windows necessarily (§4a), its tarball (lab.sh's fourth argument), to which this adds `/opt/hex`;
//! - `HEX_GUEST_RUNTIME_TOOLS`: runtime-tools built for the guest's arch (runtime_tools.sh), unpacked;
//! - `HEX_GUEST_VALIDATION_OUT`: an empty directory for the results;
//! - optionally `HEX_GUEST_CPUS` and `HEX_GUEST_MEMORY_MIB` (else the host's CPUs and Shards' least plus twice `PEAK_MIB`).
//!
//! Run: `cargo test -p hex-cli --test guest_validation -- --nocapture` with those set.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    clippy::arithmetic_side_effects,
    clippy::disallowed_macros
)]

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use hex_api::{Bounds, BoxId, BoxSpec, ClassSpec, Runtime};
use hex_runtime::guest::shares::{SHARES, Share};
use hex_runtime::guest::{GuestConfig, GuestRuntime};

/// The guest VM process's peak resident set under this test's workload, in MiB, the most of three runs
/// (HEX-GUEST-20 on adas-mac-studio.local-2, aarch64 HVF, 2026-11-06; bench/native/guest/peak-rss/RECORD.md).
const PEAK_MIB: u64 = 975;

/// How long the lab may run before the test fails: twice (the margin) the slowest run of this test on
/// record for this host's class (its `validation_run` row in bench/native/guest/records.tsv; the runs in
/// bench/native/guest/validation-runs/RECORD.md), the lead's rule for a wait that can't end on a fact
/// alone. Each run's time includes its boot and staging, so the lab's own is less. A class with no row
/// fails here by name rather than borrow another's.
fn suite() -> Duration {
    2 * hex_runtime::guest::recorded("validation_run").unwrap_or_else(|why| panic!("{why}"))
}

/// Every capability Hex's runtime names (crates/hex-runtime/src/linux/oci/caps.rs), as a privileged
/// container holds them.
const CAPS: &[&str] = &[
    "CAP_CHOWN",
    "CAP_DAC_OVERRIDE",
    "CAP_DAC_READ_SEARCH",
    "CAP_FOWNER",
    "CAP_FSETID",
    "CAP_KILL",
    "CAP_SETGID",
    "CAP_SETUID",
    "CAP_SETPCAP",
    "CAP_LINUX_IMMUTABLE",
    "CAP_NET_BIND_SERVICE",
    "CAP_NET_BROADCAST",
    "CAP_NET_ADMIN",
    "CAP_NET_RAW",
    "CAP_IPC_LOCK",
    "CAP_IPC_OWNER",
    "CAP_SYS_MODULE",
    "CAP_SYS_RAWIO",
    "CAP_SYS_CHROOT",
    "CAP_SYS_PTRACE",
    "CAP_SYS_PACCT",
    "CAP_SYS_ADMIN",
    "CAP_SYS_BOOT",
    "CAP_SYS_NICE",
    "CAP_SYS_RESOURCE",
    "CAP_SYS_TIME",
    "CAP_SYS_TTY_CONFIG",
    "CAP_MKNOD",
    "CAP_LEASE",
    "CAP_AUDIT_WRITE",
    "CAP_AUDIT_CONTROL",
    "CAP_SETFCAP",
    "CAP_MAC_OVERRIDE",
    "CAP_MAC_ADMIN",
    "CAP_SYSLOG",
    "CAP_WAKE_ALARM",
    "CAP_BLOCK_SUSPEND",
    "CAP_AUDIT_READ",
    "CAP_PERFMON",
    "CAP_BPF",
    "CAP_CHECKPOINT_RESTORE",
];

fn given(name: &str) -> Option<PathBuf> {
    std::env::var_os(name).map(PathBuf::from)
}

/// The lab's config.json: run_hex.sh's docker container in runtime-spec form (`--privileged`, its own
/// cgroup namespace, `--network=none`), its root the lab, the runtime-tools checkout and the out directory
/// bound from their host paths, which the node rewrites to their shares.
fn lab_config(lab: &Path, runtime_tools: &Path, out: &Path) -> serde_json::Value {
    let caps: Vec<&str> = CAPS.to_vec();
    serde_json::json!({
        "ociVersion": "1.2.0",
        "root": {"path": lab, "readonly": false},
        "hostname": "lab",
        "process": {
            "cwd": "/opt/runtime-tools",
            "args": ["/bin/bash", "/opt/lab/inside.sh"],
            "env": [
                "PATH=/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin",
                "RT=/opt/runtime-tools",
                "OUT=/opt/out",
                "RUNTIME=/opt/lab/hex-oci",
                "HEX_OCI_ROOT=/run/hex-oci",
                "TMPDIR=/tmp"
            ],
            "user": {"uid": 0, "gid": 0},
            "noNewPrivileges": false,
            "capabilities": {
                "bounding": caps, "effective": caps, "permitted": caps
            }
        },
        "mounts": [
            {"destination": "/proc", "type": "proc", "source": "proc"},
            {"destination": "/dev", "type": "tmpfs", "source": "tmpfs", "options": ["nosuid", "mode=755"]},
            {"destination": "/dev/pts", "type": "devpts", "source": "devpts",
             "options": ["nosuid", "noexec", "newinstance", "ptmxmode=0666", "mode=0620"]},
            {"destination": "/dev/shm", "type": "tmpfs", "source": "shm", "options": ["nosuid", "noexec", "nodev"]},
            {"destination": "/dev/mqueue", "type": "mqueue", "source": "mqueue", "options": ["nosuid", "noexec", "nodev"]},
            {"destination": "/sys", "type": "sysfs", "source": "sysfs", "options": ["nosuid", "noexec", "nodev"]},
            {"destination": "/sys/fs/cgroup", "type": "cgroup2", "source": "cgroup", "options": ["nosuid", "noexec", "nodev", "rw"]},
            {"destination": "/run", "type": "tmpfs", "source": "tmpfs", "options": ["nosuid", "nodev", "mode=755"]},
            {"destination": "/tmp", "type": "tmpfs", "source": "tmpfs", "options": ["nosuid", "nodev"]},
            {"destination": "/opt/runtime-tools", "type": "bind", "source": runtime_tools, "options": ["rbind", "ro"]},
            {"destination": "/opt/out", "type": "bind", "source": out, "options": ["rbind", "rw"]}
        ],
        "linux": {
            "namespaces": [
                {"type": "pid"}, {"type": "mount"}, {"type": "ipc"}, {"type": "uts"},
                {"type": "cgroup"}, {"type": "network"}
            ],
            "resources": {"devices": [{"allow": true, "access": "rwm"}]},
            "devices": [
                {"path": "/dev/null", "type": "c", "major": 1, "minor": 3, "fileMode": 438, "uid": 0, "gid": 0},
                {"path": "/dev/zero", "type": "c", "major": 1, "minor": 5, "fileMode": 438, "uid": 0, "gid": 0},
                {"path": "/dev/full", "type": "c", "major": 1, "minor": 7, "fileMode": 438, "uid": 0, "gid": 0},
                {"path": "/dev/random", "type": "c", "major": 1, "minor": 8, "fileMode": 438, "uid": 0, "gid": 0},
                {"path": "/dev/urandom", "type": "c", "major": 1, "minor": 9, "fileMode": 438, "uid": 0, "gid": 0},
                {"path": "/dev/tty", "type": "c", "major": 5, "minor": 0, "fileMode": 438, "uid": 0, "gid": 0}
            ]
        }
    })
}

/// A results.tsv's rows: each test's name and its result (`pass`, `fail` or `skip`).
fn results(text: &str) -> BTreeMap<String, String> {
    text.lines()
        .filter_map(|line| {
            let mut cols = line.split('\t');
            Some((cols.next()?.to_owned(), cols.next()?.to_owned()))
        })
        .collect()
}

/// Shards, counting each guest it is asked to boot.
struct Counted {
    shards: shards_api::Shards,
    boots: AtomicUsize,
}

impl shards_api::Guests for Counted {
    fn probe_gpu(
        &self,
        spec: &shards_api::GuestSpec,
        patience: Duration,
    ) -> Result<shards_api::GpuProbe, shards_api::Refusal> {
        self.shards.probe_gpu(spec, patience)
    }

    fn backend(&self) -> Result<shards_api::Backend, shards_api::Refusal> {
        self.shards.backend()
    }

    fn boot(
        &self,
        spec: &shards_api::GuestSpec,
    ) -> Result<Box<dyn shards_api::Guest>, shards_api::Refusal> {
        self.boots.fetch_add(1, Ordering::SeqCst);
        self.shards.boot(spec)
    }

    fn attach(
        &self,
        identity: &shards_api::Identity,
        spec: &shards_api::GuestSpec,
    ) -> Result<Box<dyn shards_api::Guest>, shards_api::Refusal> {
        self.shards.attach(identity, spec)
    }
}

#[test]
fn the_oci_runtime_spec_validation_passes_in_the_guest_as_on_linux() {
    let needed = [
        "HEX_GUEST_KERNEL",
        "HEX_GUEST_INITRD",
        "SHARDS_BIN",
        "HEX_GUEST_LINUX_HEX",
        "HEX_GUEST_LAB",
        "HEX_GUEST_RUNTIME_TOOLS",
        "HEX_GUEST_VALIDATION_OUT",
    ];
    let missing: Vec<&str> = needed
        .iter()
        .copied()
        .filter(|n| std::env::var_os(n).is_none())
        .collect();
    if !missing.is_empty() {
        eprintln!(
            "SKIP: no guest and lab given ({} unset)",
            missing.join(", ")
        );
        return;
    }
    let lab = given("HEX_GUEST_LAB").unwrap();
    let runtime_tools = given("HEX_GUEST_RUNTIME_TOOLS").unwrap();
    let out = given("HEX_GUEST_VALIDATION_OUT").unwrap();
    std::fs::create_dir_all(&out).unwrap();
    assert!(
        std::fs::read_dir(&out).unwrap().next().is_none(),
        "{} isn't empty",
        out.display()
    );
    let bundle = tempfile::tempdir().unwrap();
    let hex = given("HEX_GUEST_LINUX_HEX").unwrap();
    let lab = if hex_client::guest::stage::is_tarball(&lab) {
        with_hex(&lab, &hex, &bundle.path().join("lab.tar"))
    } else {
        std::fs::copy(&hex, lab.join("opt/hex")).unwrap();
        make_executable(&lab.join("opt/hex"));
        lab
    };
    std::fs::write(
        bundle.path().join("config.json"),
        serde_json::to_vec_pretty(&lab_config(&lab, &runtime_tools, &out)).unwrap(),
    )
    .unwrap();

    let number =
        |name: &str, default: u64| std::env::var(name).map_or(default, |v| v.parse().unwrap());
    // The run's two shares and nothing else (§6a): the suite's /etc binds name the lab's own /etc.
    let config = GuestConfig {
        binary: given("SHARDS_BIN").unwrap(),
        kernel: given("HEX_GUEST_KERNEL").unwrap(),
        initrd: given("HEX_GUEST_INITRD").unwrap(),
        // Unset, the host's CPUs (`host_cpus`): the fastest run, as a vCPU costs nothing while it
        // idles.
        vcpus: std::env::var("HEX_GUEST_CPUS").map_or_else(|_| host_cpus(), |v| v.parse().unwrap()),
        gpu: false,
        // Unset, Shards' least guest and twice this workload's measured peak (PEAK_MIB): the margin
        // C's guest_tree takes, stated as a factor, never below what was measured.
        memory_mib: number(
            "HEX_GUEST_MEMORY_MIB",
            shards_api::min_memory_mib(std::env::consts::ARCH) + 2 * PEAK_MIB,
        ),
        shares: vec![
            Share {
                dir: runtime_tools.clone(),
                read_only: true,
                at: format!("{SHARES}/0"),
            },
            Share {
                dir: out.clone(),
                read_only: false,
                at: format!("{SHARES}/1"),
            },
        ],
        staged: hex_client::guest::stage::STAGED.to_owned(),
    };
    let guests = Arc::new(Counted {
        shards: shards_api::Shards::new(),
        boots: AtomicUsize::new(0),
    });
    let node = GuestRuntime::new(guests.clone(), config);
    let id = BoxId::new("oci-validation-lab").unwrap();
    node.create(BoxSpec {
        route: None,
        allocation: None,
        devices: Vec::new(),
        id: id.clone(),
        bounds: Bounds::default(),
        class: ClassSpec::Bundle(bundle.path().to_owned()),
        owner: None,
    })
    .expect("create the lab in the guest");
    node.start(&id).expect("start the lab");
    // The fact waited on is the lab's end, which the node reports with its exit.
    let suite = suite();
    let read = || node.inspect(&id).expect("inspect the lab").exit;
    let exit =
        reread(suite, read).unwrap_or_else(|| panic!("the lab ran past the suite's {suite:?}"));
    eprintln!("the lab ended: {exit:?}");
    node.delete(&id).expect("delete the lab");

    let got = std::fs::read_to_string(out.join("results.tsv")).expect("the lab's results.tsv");
    let guest = results(&got);
    let linux = results(include_str!(
        "../../../bench/oci-validation/results-hex-linux.tsv"
    ));
    eprintln!("{got}");
    let lost: Vec<&String> = linux
        .iter()
        .filter(|(_, r)| r.as_str() == "pass")
        .map(|(name, _)| name)
        .filter(|name| guest.get(*name).map(String::as_str) != Some("pass"))
        .collect();
    assert!(
        lost.is_empty(),
        "tests that pass on Linux and not in the guest: {lost:?}"
    );
    assert_eq!(
        guests.boots.load(Ordering::SeqCst),
        1,
        "the node booted its guest more than once"
    );
}

/// The lab tarball `lab` copied to `out` with the guest's `hex` added at `opt/hex`, mode 0755: each of its
/// entries re-written as it came, a symbolic link with its text.
fn with_hex(lab: &Path, hex: &Path, out: &Path) -> PathBuf {
    let file = std::fs::File::open(lab).unwrap();
    let reader: Box<dyn std::io::Read> = if lab.to_str().unwrap().ends_with(".tar") {
        Box::new(file)
    } else {
        Box::new(flate2::read::GzDecoder::new(file))
    };
    let mut from = tar::Archive::new(reader);
    let mut to = tar::Builder::new(std::fs::File::create(out).unwrap());
    for entry in from.entries().unwrap() {
        let mut entry = entry.unwrap();
        let path = entry.path().unwrap().into_owned();
        let mut header = entry.header().clone();
        match header.entry_type() {
            tar::EntryType::Symlink => {
                let target = entry.link_name().unwrap().unwrap().into_owned();
                to.append_link(&mut header, &path, &target).unwrap();
            }
            _ => to.append_data(&mut header, &path, &mut entry).unwrap(),
        }
    }
    let bytes = std::fs::read(hex).unwrap();
    let mut header = tar::Header::new_gnu();
    header.set_entry_type(tar::EntryType::Regular);
    header.set_mode(0o755);
    header.set_size(u64::try_from(bytes.len()).unwrap());
    to.append_data(&mut header, "opt/hex", bytes.as_slice())
        .unwrap();
    to.into_inner().unwrap();
    out.to_owned()
}

#[cfg(unix)]
fn make_executable(path: &Path) {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).unwrap();
}

/// NTFS holds no mode: the packer stages every file 0755 from Windows.
#[cfg(not(unix))]
fn make_executable(_path: &Path) {}

/// Waits `d` at least: `park_timeout` may wake early, and `thread::sleep` is disallowed.
fn rest(d: Duration) {
    let Some(end) = Instant::now().checked_add(d) else {
        return;
    };
    loop {
        let left = end.saturating_duration_since(Instant::now());
        if left.is_zero() {
            return;
        }
        std::thread::park_timeout(left);
    }
}

/// Reads with `look` until it shows something, or `patience` has passed. The seam has no wait for a box's
/// end (`hex_api::Runtime`: `inspect` reports it once it has come), so it is read again after waiting as
/// long as the last read took, step 7's learned re-read (cp/t7-native-s7 `hex_api::differential::until`):
/// at most half the time reading, at whatever a read costs here, and no picked interval.
fn reread<T>(patience: Duration, mut look: impl FnMut() -> Option<T>) -> Option<T> {
    let deadline = Instant::now().checked_add(patience);
    loop {
        let read = Instant::now();
        let seen = look();
        let took = read.elapsed();
        if seen.is_some() || deadline.is_none_or(|d| Instant::now() >= d) {
            return seen;
        }
        rest(took);
    }
}

/// HEX-GUEST-35 (the lead's call: the poll follows the read's own time): read with a look whose reads cost
/// 1.2 s and 50 ms in turn and that shows nothing until its fourth read. Expect each re-read to wait at
/// least as long as the read before it took, and the fourth read's answer returned.
#[test]
fn a_reread_waits_as_long_as_the_last_read_took() {
    // A long read and a short one: no fixed interval, the old 500 ms with macOS's timer slack included,
    // waits as long as the long one without the test seeing it.
    let costs = [1200, 50, 1200, 50].map(Duration::from_millis);
    let mut reads = Vec::new();
    let got = reread(Duration::from_secs(60), || {
        let start = Instant::now();
        let cost = costs[reads.len()];
        let mut x = 0u64;
        while start.elapsed() < cost {
            x = std::hint::black_box(x.wrapping_add(1));
        }
        reads.push((start, Instant::now()));
        if reads.len() == costs.len() {
            Some(reads.len())
        } else {
            None
        }
    });
    assert_eq!(got, Some(4));
    for pair in reads.windows(2) {
        let ((s0, e0), (s1, _)) = (pair[0], pair[1]);
        assert!(
            s1 - e0 >= e0 - s0,
            "re-read after {:?}, the read took {:?}",
            s1 - e0,
            e0 - s0
        );
    }
}

/// The host's CPUs, as std reads them: a guest here has every one, as a vCPU costs nothing while
/// it idles.
fn host_cpus() -> u32 {
    std::thread::available_parallelism().map_or(1, |n| u32::try_from(n.get()).unwrap())
}
