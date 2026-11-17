//! Joint plan step 39, AC-T7.4.1 (docs/design/node-under-shards-init.md §9): Hex's node guest in either form, through
//! production code alone, its starts and its VM's memory read the same way in both forms (ROOT, 2026-11-13):
//! - **`image`**, Hex's node under shards-init. The test builds Hex's node image (`scripts/guest/node.Agentfile`, the
//!   guest's static `hex` its `/init`) into the node's Shards home, as `scripts/guest/node-image.sh` does, and it boots
//!   through Shards' image boot under D234's nesting grant: Hex's init is the run's own command, PID 1 of the
//!   workload's own PID namespace, and shards-init the VM's. Hex's PID 1 is intact by construction: `/init` is the
//!   image's one file, and its READY comes only once that init has started its node and relay.
//! - **`kernel`**, Hex's own PID-1 path: Shards' direct boot of `HEX_GUEST_KERNEL` and `HEX_GUEST_INITRD`, `hex` the
//!   VM's init, as the node's managed shared guest, which boots as its shared backing is granted (as the scheduler's
//!   Index grants it, through the node's socket); on Linux, whose node has no managed shared guest, as a Shards box's
//!   own VM, which boots as the box is created.
//!
//! On macOS (HVF) and Windows (WHP), the oci class's conformance suite, every case, through a live `hex node`'s socket,
//! its guest the form `HEX_NODE_GUEST_FORM` names: in the image form its oci answer books one node VM, Ready, and
//! routes its boxes to it (`Execution::NodeVm`); in the kernel form it books none. On Linux (KVM) the oci class is
//! native by design, so no Linux node boots a node VM for it (node-under-shards-init.md §9): there each form's boot is
//! measured through its own production class instead, with no suite. The image form's through `ImageClass::boot_node`,
//! as a node's guest runtime boots its node VM, with hex-node's image runtime as its record authority; the kernel
//! form's through a live `hex node`'s Shards class, one box created through its socket at its dedicated route's cost,
//! a Linux node keeping no shared guest to grant.
//!
//! Its starts, cold and restored told apart, never one figure (ROOT, 2026-11-13):
//! - **The image form.** Its run hosts boot the guest the node's Shards home stores, so D253's templates engage: never
//!   one given them (`SHARDS_KERNEL` and `SHARDS_INIT` make every run `StartedFrom::Booted`, with no template, so they
//!   must be unset). Before anything boots, Shards' own `shards guest use` keeps `HEX_NODE_GUEST_KEEP_KERNEL` and
//!   `HEX_NODE_GUEST_KEEP_INIT` in that home by content, timed on a line of its own, so no boot fetches a kernel over
//!   the network. Start 1 is then the cold one, saving its image's template; each of `HEX_NODE_GUEST_RESTARTS` more
//!   starts on the same state restores it (where its hypervisor restores no snapshot, each boots as start 1 did).
//! - **The kernel form.** One start a run, cold: a direct boot has no template, and a restart would take back the guest
//!   its node left running rather than boot one, so a cold start is a fresh run.
//!
//! Each start prints (`NODE-GUEST`):
//! - its start: from the node's spawn to its ready line (in the image form its node VM booted, ready and greeted by
//!   then), in the kernel form then its shared guest's grant to Live, and on macOS and Windows its first create through
//!   the socket; on Linux's image form, `boot_node`'s own READY time and its call's, and how its run started; on Linux's
//!   kernel form, its box's create (its VM booted, its node and relay up, the box made in it);
//! - the memory of the VM's processes as their OS counts it, read once each is up and, at start 1 on macOS and Windows,
//!   once the suite is done: macOS's physical footprint and its peak (proc_pid_rusage); Windows' private working set
//!   (its perf counters) and its peak working set (Win32_Process, through PowerShell's CIM); Linux's anonymous resident
//!   pages and its resident peak (`/proc/<pid>/status`'s `RssAnon` and `VmHWM`). Beside it, Hex's own reading: the
//!   node VM's host use (Shards' D241 reading) on Linux, and in the kernel form the shared guest's backing usage, on
//!   Linux its box's usage;
//! - in the image form, the node's census answers for its node VM's boot (`hex.image.boot`) and READY
//!   (`guest.hosted.ready`), where a node runs.
//!
//! Before a restart, the start before it ends (its node killed, as a crash ends it, or on Linux its node VM stopped)
//! and the test waits for each of its processes' ends, by their lives, so no VM ending overlaps the next start's
//! measure. On Windows the node's keeper, which outlives a killed node by design and never ends on its own, is ended
//! by the node's teardown too (image_node's `Node`). The kernel form's shared guest is released through the node's
//! socket before its node ends; on Linux its box is deleted, and its node's processes' ends are waited for.
//!
//! Explicit inputs only, each by its own variable, a missing one failing by name:
//! - `HEX_NODE_GUEST_FORM`: `image` or `kernel`; `HEX_NODE_GUEST_WORK`: a directory of this run's own, made here;
//! - `HEX_GUEST_LINUX_HEX`: the guest's static Linux `hex`, at `/hex` in each box's root (guest_real's fixture) and, in
//!   the image form, the node image's `/init`;
//! - the node's guest configuration as `hex node` reads it: `SHARDS_BIN`, `HEX_GUEST_CPUS`, `HEX_GUEST_MEMORY_MIB`,
//!   `HEX_GUEST_COST_FILE`, `HEX_GUEST_COST_SHA256` and `HEX_GUEST_COST_EVIDENCE`; in the kernel form
//!   `HEX_GUEST_KERNEL` and `HEX_GUEST_INITRD`, and in the image form neither (a node boots one form);
//! - in the image form, `HEX_NODE_GUEST_KEEP_KERNEL` and `HEX_NODE_GUEST_KEEP_INIT` (Shards' pinned kernel and the
//!   shards-init its `shards` build carries) and `HEX_NODE_GUEST_RESTARTS`;
//! - `HEX_WAITS_CENSUS_MAX`: the census's most lines.
//!
//! No wait of its own is a timer: each call waits for its answer while the node's connection stands, the node's start
//! waits for its ready line while the node runs (image_node's `Node::start`), and a start's end waits for its
//! processes'.
//!
//! Run once a form, where Shards boots VMs: `cargo test --release --locked -p hex-cli --test node_guest_real --
//! --ignored --nocapture`; on Linux inside a cgroup delegated to the test binary (`HEX_DELEGATED`), as image_real is.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    clippy::arithmetic_side_effects,
    clippy::disallowed_macros
)]

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
#[cfg(any(target_os = "macos", windows))]
use std::time::Duration;
use std::time::Instant;

use hex_api::conformance::Act;
#[cfg(any(target_os = "macos", windows))]
use hex_api::{BackingGrant, BackingKey, BackingPhase};
use hex_api::{CapacityResources, Class, Program, Runtime};
use hex_client::Client;

// image_real's real-node helpers: this test takes the node, the image's build and the inputs.
#[allow(dead_code)]
#[path = "image_node/mod.rs"]
mod image_node;
use image_node::{Node, built, input, number, shards_dirs};

/// The name Hex's node image is tagged with in the node's Shards home; it boots pinned by its digest.
const NODE_IMAGE: &str = "hex-node-guest-real/node";

/// The node's guest, as `HEX_NODE_GUEST_FORM` names it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Form {
    Image,
    Kernel,
}

impl Form {
    fn given() -> Self {
        match input("HEX_NODE_GUEST_FORM").as_str() {
            "image" => Self::Image,
            "kernel" => Self::Kernel,
            other => panic!("HEX_NODE_GUEST_FORM is {other:?}: image or kernel"),
        }
    }

    fn word(self) -> &'static str {
        match self {
            Self::Image => "image",
            Self::Kernel => "kernel",
        }
    }

    /// What each form needs of the run's environment, and the starts it makes: the image form's restarts restore the
    /// template its first start saved.
    fn checked(self) -> u64 {
        match self {
            Self::Image => {
                for name in [
                    "HEX_GUEST_KERNEL",
                    "HEX_GUEST_INITRD",
                    "SHARDS_KERNEL",
                    "SHARDS_INIT",
                ] {
                    assert!(
                        std::env::var_os(name).is_none(),
                        "{name} is set: the image form boots no kernel of Hex's own, and its run hosts boot the \
                         guest the node's Shards home keeps, whose templates D253 saves and restores"
                    );
                }
                1 + number("HEX_NODE_GUEST_RESTARTS")
            }
            Self::Kernel => {
                file("HEX_GUEST_KERNEL");
                file("HEX_GUEST_INITRD");
                1
            }
        }
    }
}

/// A file a run needs, named by its variable.
fn file(name: &str) -> PathBuf {
    let path = PathBuf::from(input(name));
    assert!(path.is_file(), "{name} {path:?} is no file");
    path
}

/// Keeps `HEX_NODE_GUEST_KEEP_KERNEL` and `HEX_NODE_GUEST_KEEP_INIT` as the guest the node's Shards home boots, by
/// content, through Shards' own `shards guest use`, timed on a line of its own: its run hosts then boot that stored
/// guest, whose templates D253 saves and restores, and no boot fetches a kernel.
fn keep_guest(shards: &Path, work: &Path) {
    let (kernel, init) = (
        file("HEX_NODE_GUEST_KEEP_KERNEL"),
        file("HEX_NODE_GUEST_KEEP_INIT"),
    );
    let (home, cache) = shards_dirs(&work.join("state"));
    fs::create_dir_all(&home).unwrap();
    let began = Instant::now();
    let kept = Command::new(shards)
        .args(["guest", "use", "--kernel"])
        .arg(&kernel)
        .arg("--init")
        .arg(&init)
        .env("SHARDS_HOME", &home)
        .env("SHARDS_CACHE", &cache)
        .output()
        .unwrap();
    let took = began.elapsed();
    assert!(
        kept.status.success(),
        "shards guest use: {}; its stderr:\n{}",
        kept.status,
        String::from_utf8_lossy(&kept.stderr).trim_end()
    );
    println!(
        "NODE-GUEST guest_kept us={} {}",
        took.as_micros(),
        String::from_utf8_lossy(&kept.stdout)
            .split_whitespace()
            .collect::<Vec<_>>()
            .join(" ")
    );
}

/// Hex's node image (docs/design/node-under-shards-init.md §4) built into the node's Shards home as node-image.sh
/// builds it, from a context of its Agentfile and the guest's `hex`: the image pinned by the manifest digest its build
/// says.
fn node_image(shards: &Path, hex: &Path, work: &Path) -> String {
    let context = work.join("node-image");
    fs::create_dir_all(&context).unwrap();
    let agentfile =
        Path::new(env!("CARGO_MANIFEST_DIR")).join("../../scripts/guest/node.Agentfile");
    fs::copy(&agentfile, context.join("Agentfile")).unwrap();
    fs::copy(hex, context.join("hex")).unwrap();
    let image = built(shards, &context, work, NODE_IMAGE);
    println!("NODE-GUEST image {image}");
    image
}

/// One process and the memory its OS counts for it, in bytes: now and at its peak.
#[derive(Debug, Clone)]
struct Held {
    pid: u32,
    parent: u32,
    name: String,
    now: Option<u64>,
    peak: Option<u64>,
}

/// Every process macOS lists now (`ps -A -o pid=,ppid=,comm=`), each with its physical footprint and its peak
/// (proc_pid_rusage's `ri_phys_footprint` and `ri_lifetime_max_phys_footprint`): none for one gone since.
#[cfg(target_os = "macos")]
fn listed() -> Vec<Held> {
    let ps = Command::new("ps")
        .args(["-A", "-o", "pid=,ppid=,comm="])
        .output()
        .unwrap();
    assert!(ps.status.success(), "ps: {}", ps.status);
    String::from_utf8_lossy(&ps.stdout)
        .lines()
        .filter_map(|line| {
            let mut words = line.split_whitespace();
            let pid: u32 = words.next()?.parse().ok()?;
            let parent: u32 = words.next()?.parse().ok()?;
            let name = words.collect::<Vec<_>>().join(" ");
            let usage = hex_runtime::macos::sys::rusage(i32::try_from(pid).ok()?)
                .ok()
                .flatten()
                .filter(|usage| !usage.ended);
            Some(Held {
                pid,
                parent,
                name,
                now: usage.as_ref().map(|usage| usage.footprint),
                peak: usage.as_ref().map(|usage| usage.peak),
            })
        })
        .collect()
}

/// Every process Windows lists now (Win32_Process: its id, its parent's, its peak working set in KiB and its name),
/// each with its private working set from the perf counters (Win32_PerfRawData_PerfProc_Process's
/// `WorkingSetPrivate`, bytes), through PowerShell's CIM, one line each; `-` where the counters list it not.
#[cfg(windows)]
fn listed() -> Vec<Held> {
    let script = "$private = @{}; Get-CimInstance Win32_PerfRawData_PerfProc_Process | ForEach-Object { \
                  $private[[uint32]$_.IDProcess] = $_.WorkingSetPrivate }; Get-CimInstance Win32_Process | \
                  ForEach-Object { $p = [uint32]$_.ProcessId; '{0} {1} {2} {3} {4}' -f $p, $_.ParentProcessId, \
                  $(if ($private.ContainsKey($p)) { $private[$p] } else { '-' }), $_.PeakWorkingSetSize, $_.Name }";
    let ps = Command::new("powershell")
        .args(["-NoProfile", "-Command", script])
        .output()
        .unwrap();
    assert!(ps.status.success(), "powershell: {}", ps.status);
    String::from_utf8_lossy(&ps.stdout)
        .lines()
        .filter_map(|line| {
            let mut words = line.split_whitespace();
            let pid: u32 = words.next()?.parse().ok()?;
            let parent: u32 = words.next()?.parse().ok()?;
            let now = words.next()?.parse().ok();
            let peak = words
                .next()?
                .parse::<u64>()
                .ok()
                .and_then(|kib| kib.checked_mul(1024));
            Some(Held {
                pid,
                parent,
                name: words.collect::<Vec<_>>().join(" "),
                now,
                peak,
            })
        })
        .collect()
}

/// Every process Linux lists now, each with its anonymous resident pages and its resident peak
/// (`/proc/<pid>/status`'s `RssAnon` and `VmHWM`); none for a kernel thread, which has neither.
#[cfg(target_os = "linux")]
fn listed() -> Vec<Held> {
    let mut out = Vec::new();
    for entry in fs::read_dir("/proc").unwrap().flatten() {
        let Some(pid) = entry
            .file_name()
            .to_str()
            .and_then(|name| name.parse::<u32>().ok())
        else {
            continue;
        };
        // A process gone since the directory was read has no status.
        let Ok(status) = fs::read_to_string(entry.path().join("status")) else {
            continue;
        };
        let field = |key: &str| {
            status
                .lines()
                .find_map(|line| line.strip_prefix(key))
                .map(str::trim)
        };
        let bytes = |key: &str| {
            field(key)
                .and_then(|value| value.strip_suffix(" kB"))
                .and_then(|kib| kib.trim().parse::<u64>().ok())
                .and_then(|kib| kib.checked_mul(1024))
        };
        let (Some(parent), Some(name)) = (
            field("PPid:").and_then(|parent| parent.parse().ok()),
            field("Name:"),
        ) else {
            continue;
        };
        out.push(Held {
            pid,
            parent,
            name: name.to_owned(),
            now: bytes("RssAnon:"),
            peak: bytes("VmHWM:"),
        });
    }
    out
}

/// The processes descended from `root` now, its children's children among them.
fn tree(root: u32) -> Vec<Held> {
    let all = listed();
    let mut found: Vec<Held> = Vec::new();
    let mut parents = vec![root];
    while let Some(parent) = parents.pop() {
        for held in all
            .iter()
            .filter(|held| held.parent == parent && held.pid != parent)
        {
            if !found.iter().any(|seen| seen.pid == held.pid) {
                parents.push(held.pid);
                found.push(held.clone());
            }
        }
    }
    found
}

/// Says `tree`'s processes and their memory at `at` of start `start`, and their sum where every one was read.
fn say_memory(start: u64, at: &str, tree: &[Held]) {
    let word = |bytes: Option<u64>| bytes.map_or_else(|| "unread".to_owned(), |b| b.to_string());
    for held in tree {
        println!(
            "NODE-GUEST memory start={start} at={at} pid={} parent={} now_bytes={} peak_bytes={} name={}",
            held.pid,
            held.parent,
            word(held.now),
            word(held.peak),
            held.name
        );
    }
    let sum = tree.iter().try_fold(0u64, |sum, held| {
        held.now.and_then(|now| sum.checked_add(now))
    });
    println!(
        "NODE-GUEST memory start={start} at={at} processes={} now_bytes_sum={}",
        tree.len(),
        word(sum)
    );
}

/// Waits for each of `pids` to end, by its life: a kqueue's `NOTE_EXIT` on macOS, which any process takes, no timer.
#[cfg(target_os = "macos")]
fn ended(pids: &[u32]) {
    use hex_runtime::macos::sys::{Event, Kqueue};
    let kqueue = Kqueue::new().unwrap();
    let mut left: Vec<i32> = Vec::new();
    for pid in pids {
        let pid = i32::try_from(*pid).unwrap();
        if !kqueue.watch_end(pid).unwrap() {
            left.push(pid);
        }
    }
    let mut events = Vec::new();
    while !left.is_empty() {
        kqueue.wait(None, None, &mut events).unwrap();
        for event in &events {
            if let Event::Exit { pid, .. }
            | Event::Fork {
                pid, exited: true, ..
            } = event
            {
                left.retain(|left| left != pid);
            }
        }
    }
}

/// Waits for each of `pids` to end, by its life: PowerShell's `Wait-Process`, which waits on each process's handle with
/// no timeout given; one already gone is passed over (its error silenced, so the command's own exit is 0).
#[cfg(windows)]
fn ended(pids: &[u32]) {
    if pids.is_empty() {
        return;
    }
    let ids = pids
        .iter()
        .map(u32::to_string)
        .collect::<Vec<_>>()
        .join(",");
    let waited = Command::new("powershell")
        .args([
            "-NoProfile",
            "-Command",
            &format!("Wait-Process -Id {ids} -ErrorAction SilentlyContinue; exit 0"),
        ])
        .status()
        .unwrap();
    assert!(waited.success(), "Wait-Process: {waited}");
}

/// The keeper a Windows node starts as its own child to hold its boxes' jobs (hex-runtime's `windows::keeper`), named
/// by `keeper.owner` in the `keeper` folder of the node's `--state` (hex-cli's `node.rs`): none where that names none,
/// or where the process it names is gone.
#[cfg(windows)]
fn keeper(work: &Path) -> Option<hex_runtime::windows::keeper::NodeKeeper> {
    use hex_runtime::windows::keeper::{NodeKeeper, keeper_file};
    let keeping = work.join("state").join("keeper");
    let owner = fs::read_to_string(keeper_file(&keeping)).ok()?;
    let mut words = owner.split_whitespace();
    let (Some(Ok(pid)), Some(Ok(created))) = (
        words.next().map(str::parse::<u32>),
        words.next().map(str::parse::<u64>),
    ) else {
        panic!("{:?} names no keeper: {owner:?}", keeper_file(&keeping));
    };
    match NodeKeeper::open(pid, created, &keeping) {
        Ok(keeper) => Some(keeper),
        // No process of its pid and creation time runs: it's gone.
        Err(hex_api::Refusal::Invalid { .. }) => None,
        Err(e) => panic!("opening the node's keeper, pid {pid}: {e}"),
    }
}

/// Waits for each of `pids` to end, by its life: its pidfd, which reads ready once it has ended whoever reaps it; one
/// already gone has none to open.
#[cfg(target_os = "linux")]
fn ended(pids: &[u32]) {
    use rustix::event::{PollFd, PollFlags};
    use rustix::process::{Pid, PidfdFlags};
    for pid in pids {
        let Some(pid) = i32::try_from(*pid).ok().and_then(Pid::from_raw) else {
            continue;
        };
        let Ok(fd) = rustix::process::pidfd_open(pid, PidfdFlags::empty()) else {
            continue;
        };
        let mut fds = [PollFd::new(&fd, PollFlags::IN)];
        rustix::event::poll(&mut fds, None).unwrap();
    }
}

/// The node's census lines past the first `seen` that answer its node VM's boot or READY (`name nanos`, a refusal
/// `!nanos`, a run-out `>nanos`), said as they are: where a node boots one (macOS, Windows).
#[cfg(any(target_os = "macos", windows))]
fn say_census(start: u64, census: &Path, seen: &mut usize) {
    let text = fs::read_to_string(census).unwrap_or_default();
    let lines: Vec<&str> = text.lines().collect();
    for line in lines.iter().skip(*seen) {
        for wait in ["hex.image.boot", "guest.hosted.ready"] {
            if line
                .strip_prefix(wait)
                .is_some_and(|rest| rest.starts_with(' '))
            {
                println!("NODE-GUEST census start={start} {line}");
            }
        }
    }
    *seen = lines.len();
}

/// A box root as a tarball in `dir`, its one file `/hex` mode 0755 (guest_real's fixture), staged from its own entries
/// on every host (docs/design/shards-guest.md §4a).
fn hex_root(dir: &Path, hex: &Path) -> PathBuf {
    let rootfs = dir.join("rootfs.tar");
    let hex = fs::read(hex).unwrap();
    let mut tarball = tar::Builder::new(fs::File::create(&rootfs).unwrap());
    let mut header = tar::Header::new_gnu();
    header.set_entry_type(tar::EntryType::Regular);
    header.set_mode(0o755);
    header.set_size(u64::try_from(hex.len()).unwrap());
    tarball
        .append_data(&mut header, "hex", hex.as_slice())
        .unwrap();
    tarball.into_inner().unwrap();
    rootfs
}

/// The program of `act`, as `hex_root`'s `/hex` runs it: its selftest act.
fn act(act: &Act) -> Program {
    let mut args = vec!["/hex".to_owned(), "selftest-box".to_owned()];
    args.extend(match act {
        Act::Say(words) => vec!["say".to_owned(), (*words).to_owned()],
        Act::Fail(code) => vec!["fail".to_owned(), code.to_string()],
        Act::Wait => vec!["wait".to_owned()],
        Act::Hog(bytes) => vec!["hog".to_owned(), bytes.to_string()],
        Act::Spawn(n) => vec!["spawn".to_owned(), n.to_string()],
        Act::Echo => vec!["echo".to_owned()],
    });
    let args: Vec<&str> = args.iter().map(String::as_str).collect();
    Program::of(&args)
}

/// The kernel form's shared guest booted as the scheduler's Index boots it: the node's one Prepared shared backing
/// granted through its socket at its cost's allocation, timed to Live. Its key, and how long its grant took.
#[cfg(any(target_os = "macos", windows))]
fn grant_shared(client: &Client) -> (BackingKey, Duration) {
    let capacity = client.capacity(Class::Oci).unwrap().unwrap();
    let prepared: Vec<_> = capacity
        .backings
        .iter()
        .filter(|offer| offer.phase == BackingPhase::Prepared)
        .collect();
    let [offer] = prepared.as_slice() else {
        panic!("one Prepared shared backing: {:?}", capacity.backings);
    };
    let grant = BackingGrant {
        key: offer.key,
        allocation: offer.cost.allocate(CapacityResources::default()).unwrap(),
        devices: Vec::new(),
    };
    let began = Instant::now();
    let live = client.initialize_backing(grant).unwrap();
    let took = began.elapsed();
    assert_eq!(live.phase, BackingPhase::Live, "{live:?}");
    (offer.key, took)
}

/// The shared guest's use as Hex reads it (its backing's usage), said beside its OS's reading.
#[cfg(any(target_os = "macos", windows))]
fn say_shared_usage(client: &Client, key: BackingKey, start: u64) {
    let usage = client.backing_usage(&key).unwrap();
    println!(
        "NODE-GUEST backing_usage start={start} memory_bytes={} memory_peak_bytes={} cpu_usec={}",
        usage.memory_bytes, usage.memory_peak_bytes, usage.cpu_usec
    );
}

/// The shared guest released through the node's socket, as the Index releases a backing, so none outlives the run.
#[cfg(any(target_os = "macos", windows))]
fn release_shared(client: &Client, key: BackingKey) {
    let released = client.release_backing(key).unwrap();
    println!("NODE-GUEST released phase={:?}", released.phase);
}

/// macOS's and Windows' half: the oci suite through a live `hex node`'s socket.
#[cfg(any(target_os = "macos", windows))]
mod suite {
    use super::*;
    use hex_api::conformance::Fixture;
    use hex_api::{
        Bounds, BoxId, BoxSpec, ClassSpec, Execution, Network, NodeVmPhase, OciSpec, Rootfs,
    };

    /// `oci` boxes whose root holds the Linux `hex` at `/hex` alone, their programs its selftest acts (guest_real's).
    pub struct OciBoxes(PathBuf);

    impl OciBoxes {
        /// The boxes' root, [`hex_root`]'s in `dir`.
        pub fn new(dir: &Path, hex: &Path) -> Self {
            Self(hex_root(dir, hex))
        }

        fn box_spec(&self, id: BoxId, bounds: Bounds, init: &Act) -> BoxSpec {
            BoxSpec {
                route: None,
                allocation: None,
                devices: Vec::new(),
                id,
                bounds,
                class: ClassSpec::Oci(Box::new(OciSpec {
                    rootfs: Rootfs::Dir(self.0.clone()),
                    init: act(init),
                    mounts: Vec::new(),
                    hostname: None,
                    network: Network::Private,
                    hosts: Vec::new(),
                })),
                owner: None,
                band: 0,
            }
        }
    }

    impl Fixture for OciBoxes {
        fn spec(&self, id: BoxId, bounds: Bounds) -> BoxSpec {
            self.box_spec(id, bounds, &Act::Wait)
        }

        fn init(&self, id: BoxId, act: &Act) -> BoxSpec {
            self.box_spec(id, Bounds::default(), act)
        }

        fn program(&self, a: &Act) -> Program {
            act(a)
        }
    }

    /// A start of the node on `work`'s state, measured: its spawn to its ready line; in the kernel form its shared
    /// guest granted to Live; then its first create; and its tree's memory once it is up. Checks its oci answer is its
    /// form's: one node VM Ready and its route in the image form, none in the kernel form. The kernel form's shared
    /// backing, where granted.
    pub fn start(
        start: u64,
        form: Form,
        work: &Path,
        fixture: &OciBoxes,
        set: &[(&str, &std::ffi::OsStr)],
    ) -> (Node, Client, Option<BackingKey>) {
        let began = Instant::now();
        let node = Node::start_with(work, None, set);
        let ready = began.elapsed();
        let client = Client::connect(&node.socket).unwrap();
        assert!(
            client.classes().contains(&Class::Oci),
            "{:?}",
            client.classes()
        );
        let shared = match form {
            Form::Kernel => {
                let (key, took) = grant_shared(&client);
                println!(
                    "NODE-GUEST shared_guest start={start} live_us={}",
                    took.as_micros()
                );
                Some(key)
            }
            Form::Image => None,
        };
        let id = BoxId::new(&format!("node-guest-first-{start}")).unwrap();
        let creating = Instant::now();
        client
            .create(fixture.init(id.clone(), &Act::Say("first")))
            .unwrap();
        let created = creating.elapsed();
        let to_first = began.elapsed();
        client.delete(&id).unwrap();
        println!(
            "NODE-GUEST start={start} form={} ready_us={} first_create_us={} to_first_create_us={}",
            form.word(),
            ready.as_micros(),
            created.as_micros(),
            to_first.as_micros()
        );
        let capacity = client.capacity(Class::Oci).unwrap().unwrap();
        match form {
            Form::Image => {
                let ready: Vec<_> = capacity
                    .node_vms
                    .iter()
                    .filter(|receipt| receipt.phase == NodeVmPhase::Ready)
                    .collect();
                assert_eq!(ready.len(), 1, "one node VM Ready: {:?}", capacity.node_vms);
                assert!(
                    capacity
                        .routes
                        .iter()
                        .any(|offer| offer.key.execution == Execution::NodeVm),
                    "the node VM's route: {:?}",
                    capacity.routes
                );
                for receipt in ready {
                    println!(
                        "NODE-GUEST node_vm start={start} run={} booking={:?} guest={:?}",
                        receipt
                            .run
                            .bytes()
                            .iter()
                            .map(|byte| format!("{byte:02x}"))
                            .collect::<String>(),
                        receipt.booking,
                        receipt.guest
                    );
                }
            }
            Form::Kernel => assert!(
                capacity.node_vms.is_empty(),
                "the direct boot books no node VM: {:?}",
                capacity.node_vms
            ),
        }
        if let Some(key) = shared {
            say_shared_usage(&client, key, start);
        }
        say_memory(start, "up", &tree(node.child.id()));
        (node, client, shared)
    }

    /// Ends a start's node by its teardown (image_node's `Node`, which kills it as a crash ends it and, on Windows,
    /// ends its keeper too): the processes its tree had, each still to be waited for.
    pub fn end(node: Node) -> Vec<u32> {
        let processes: Vec<u32> = tree(node.child.id()).iter().map(|held| held.pid).collect();
        drop(node);
        processes
    }

    /// Ends a start's node ([`end`]) and waits for each process of its tree to end.
    pub fn stop(node: Node) {
        ended(&end(node));
    }
}

#[cfg(any(target_os = "macos", windows))]
#[test]
#[ignore = "native: HVF or WHP, a Shards guest configuration, and in the image form Shards' kernel and init to keep"]
fn w39_the_oci_suite_passes_through_a_hex_node_in_either_guest_form() {
    use hex_api::conformance;
    let form = Form::given();
    let starts = form.checked();
    let work = PathBuf::from(input("HEX_NODE_GUEST_WORK"));
    assert!(!work.exists(), "a run's own directory: {work:?} exists");
    fs::create_dir_all(work.join("state")).unwrap();
    let hex = file("HEX_GUEST_LINUX_HEX");
    let fixture = suite::OciBoxes::new(&work, &hex);
    let census = work.join("census.txt");
    let mut seen = 0;
    let image = match form {
        Form::Image => {
            let shards = file("SHARDS_BIN");
            keep_guest(&shards, &work);
            Some(std::ffi::OsString::from(node_image(&shards, &hex, &work)))
        }
        Form::Kernel => None,
    };
    let set: Vec<(&str, &std::ffi::OsStr)> = image
        .iter()
        .map(|image| (hex_runtime::guest::ENV_IMAGE, image.as_os_str()))
        .collect();
    let (node, client, shared) = suite::start(1, form, &work, &fixture, &set);
    say_census(1, &census, &mut seen);
    let began = Instant::now();
    let failures = conformance::run(&client, &fixture);
    println!(
        "NODE-GUEST suite start=1 form={} cases={} failed={} us={}",
        form.word(),
        conformance::CASES.len(),
        failures.len(),
        began.elapsed().as_micros()
    );
    say_memory(1, "suite", &tree(node.child.id()));
    if let Some(key) = shared {
        say_shared_usage(&client, key, 1);
    }
    let left = client.list().unwrap();
    if let Some(key) = shared {
        release_shared(&client, key);
    }
    drop(client);
    suite::stop(node);
    assert!(
        failures.is_empty(),
        "oci boxes through hex node's socket, its guest's {} form: {failures:#?}",
        form.word()
    );
    assert!(left.is_empty(), "a box is left: {left:?}");
    for start in 2..=starts {
        let (node, client, _) = suite::start(start, form, &work, &fixture, &set);
        say_census(start, &census, &mut seen);
        drop(client);
        suite::stop(node);
    }
}

/// AC-T7.4.1, item 4 on Windows (joint plan step 39): a start's end leaves no process of its node running, so its wait
/// for the processes of the node's tree ends, and the next start's measure carries none of them. A Windows node starts
/// its keeper as its own child, which outlives a killed node by design and never ends on its own (hex-runtime's
/// `windows::keeper`). Do start a node of this build, no guest given, and end it as a start ends (its node's teardown,
/// image_node's `Node`); expect its keeper ended.
#[cfg(windows)]
#[test]
#[ignore = "native Windows: a hex node of this build, no VM"]
fn w39_a_starts_end_on_windows_ends_its_nodes_keeper() {
    let work = PathBuf::from(input("HEX_NODE_GUEST_WORK"));
    assert!(!work.exists(), "a run's own directory: {work:?} exists");
    fs::create_dir_all(work.join("state")).unwrap();
    let node = Node::start_with(&work, None, &[]);
    let keeper = keeper(&work).expect("a Windows node's keeper, named by its state's keeper.owner");
    let processes = suite::end(node);
    let running = keeper.alive();
    // Asked to stop either way, so a run that fails leaves it no more running than one that passes.
    keeper.stop();
    assert!(
        !running,
        "the node's keeper, pid {}, runs on after its start's end, and it never ends on its own: the start's wait for \
         its node's processes {processes:?} would never end",
        keeper.pid()
    );
    ended(&processes);
}

/// Linux's half: each form's boot through its own production class, the oci class being native there.
#[cfg(target_os = "linux")]
#[test]
#[ignore = "native: KVM, a Shards guest configuration, and in the image form Shards' kernel and init to keep"]
fn w39_each_guest_forms_boot_is_measured_through_its_production_class_on_kvm() {
    assert!(
        std::env::var_os("HEX_DELEGATED").is_some(),
        "run inside a cgroup delegated to this binary (HEX_DELEGATED)"
    );
    let form = Form::given();
    let starts = form.checked();
    let work = PathBuf::from(input("HEX_NODE_GUEST_WORK"));
    assert!(!work.exists(), "a run's own directory: {work:?} exists");
    fs::create_dir_all(work.join("state")).unwrap();
    println!("NODE-GUEST form={} starts={starts}", form.word());
    match form {
        Form::Image => kvm::image(&work, &file("HEX_GUEST_LINUX_HEX"), starts),
        Form::Kernel => kvm::kernel(&work, &file("HEX_GUEST_LINUX_HEX")),
    }
}

#[cfg(target_os = "linux")]
mod kvm {
    use super::*;
    use hex_api::{Bounds, BoxId, BoxSpec, ClassSpec, Execution, Network, OciSpec, Rootfs};
    use hex_core::stream::files::FILES_PORT;
    use hex_core::stream::{CONTROL_PORT, STAMPS_PORT, STDIO_PORT, socket_name};
    use hex_runtime::devices::Books as _;

    /// The image form: `boots` node VMs booted one after another through `ImageClass::boot_node`, as a node's guest
    /// runtime boots its node VM, their records hex-node's image runtime's: each timed to READY, its host use read,
    /// stopped, and its processes' ends waited for.
    pub fn image(work: &Path, hex: &Path, boots: u64) {
        let shards = file("SHARDS_BIN");
        keep_guest(&shards, work);
        let image = node_image(&shards, hex, work);
        let (home, cache) = shards_dirs(&work.join("state"));
        let provider = shards_api::Shards::configured(cache, home).unwrap();
        let paths = provider.runtime_paths().try_clone().unwrap();
        let runtime =
            hex_node::images::ImageRuntime::new(shards, paths, &work.join("records")).unwrap();
        // The node VM's configuration as a node reads it, its form the image just built.
        let inputs = hex_runtime::guest::GuestConfig::from_vars_with_native_inputs(|name| {
            if name == hex_runtime::guest::ENV_IMAGE {
                Some(std::ffi::OsString::from(&image))
            } else {
                std::env::var_os(name)
            }
        })
        .unwrap();
        // Its receipt, as a node's guest runtime charges it: the configured VM under the guest's cost profile.
        let allocation = inputs.cost.allocate(CapacityResources::default()).unwrap();
        let size = allocation.vm_size().unwrap();
        assert_eq!(
            (size.vcpus, size.memory_mib),
            (inputs.config.vcpus, inputs.config.memory_mib),
            "the guest's cost profile sizes the configured VM"
        );
        let shares: Vec<shards_api::image::ImageShare> = inputs
            .config
            .shares
            .iter()
            .map(|share| shards_api::image::ImageShare {
                host: share.dir.clone(),
                at: share.at.clone(),
                read_only: share.read_only,
                // As the node's guest runtime gives them (guest/mod.rs): run from, and the run's own root alone.
                exec: true,
                members: None,
            })
            .collect();
        let identity = shards_api::control::Identity::generate().unwrap();
        // One lane for each listener of the relay's (hex_core::stream::socket_name), as many as a connection of each
        // takes at once.
        let lanes = u32::try_from(
            [CONTROL_PORT, STDIO_PORT, FILES_PORT, STAMPS_PORT]
                .into_iter()
                .filter(|port| socket_name(*port).is_some())
                .count(),
        )
        .unwrap();
        for boot in 1..=boots {
            let vm = hex_runtime::guest::NodeVm {
                run: runtime.mint_node_run().unwrap(),
                image: image.clone(),
                allocation,
                shares: shares.clone(),
                controller: identity.fingerprint(),
                lanes,
            };
            let began = Instant::now();
            let booted = runtime.class().boot_node(&vm).unwrap();
            println!(
                "NODE-GUEST start={boot} form=image ready_us={} boot_node_us={} ready={:?} started_from={:?}",
                booted.took.as_micros(),
                began.elapsed().as_micros(),
                booted.ready,
                booted.started_from
            );
            let used = runtime.class().host_use_node(vm.run).unwrap();
            println!(
                "NODE-GUEST host_use start={boot} footprint_bytes={} resident_bytes={} cpu_ns={} window_us={}",
                used.footprint_bytes,
                used.resident_bytes,
                used.cpu_ns,
                used.window.as_micros()
            );
            let processes = tree(std::process::id());
            say_memory(boot, "up", &processes);
            let stopping = Instant::now();
            let exit = runtime.class().stop_node(vm.run).unwrap();
            println!(
                "NODE-GUEST stop start={boot} us={} exit={exit:?}",
                stopping.elapsed().as_micros()
            );
            ended(&processes.iter().map(|held| held.pid).collect::<Vec<_>>());
        }
    }

    /// The kernel form: Hex's own PID-1 path through the class a Linux node runs it in, its Shards class. A Linux node
    /// has no managed shared guest: its oci class is native (hex-node's `Node::backend` gives it no backend), and its
    /// Shards guest prepares a shared backing only where one is retained (`Node::install_backends`). So one Shards box,
    /// created through a live `hex node`'s socket at its dedicated route's cost (the configured VM, as the image form's
    /// receipt is): its create boots the box's own VM from `HEX_GUEST_KERNEL` and `HEX_GUEST_INITRD`, `hex` its init,
    /// and makes the box in it, timed to its answer. Its use read, as its OS counts its processes and as Hex reads it,
    /// then the box deleted and each process of the node's tree waited for.
    pub fn kernel(work: &Path, hex: &Path) {
        let boxes = image_node::own_cgroup("hex-node-guest-real").join("hex-node-guest-node");
        let rootfs = hex_root(work, hex);
        let began = Instant::now();
        let node = Node::start_with(work, Some(&boxes), &[]);
        let ready = began.elapsed();
        let client = Client::connect(&node.socket).unwrap();
        let capacity = client.capacity(Class::Shards).unwrap().unwrap();
        let cost = capacity
            .routes
            .iter()
            .find(|offer| {
                offer.key.execution == Execution::ShardsDedicated && offer.backing.is_none()
            })
            .and_then(|offer| offer.cost)
            .unwrap_or_else(|| {
                panic!(
                    "the Shards class's dedicated route, with its cost: {:?}",
                    capacity.routes
                )
            });
        let id = BoxId::new("node-guest-kernel").unwrap();
        let spec = BoxSpec {
            route: None,
            allocation: Some(cost.allocate(CapacityResources::default()).unwrap()),
            devices: Vec::new(),
            id: id.clone(),
            bounds: Bounds::default(),
            class: ClassSpec::Shards(Box::new(OciSpec {
                rootfs: Rootfs::Dir(rootfs),
                init: act(&Act::Say("first")),
                mounts: Vec::new(),
                hostname: None,
                network: Network::Private,
                hosts: Vec::new(),
            })),
            owner: None,
            band: 0,
        };
        let creating = Instant::now();
        client.create(spec).unwrap();
        let created = creating.elapsed();
        println!(
            "NODE-GUEST start=1 form=kernel ready_us={} create_us={} to_created_us={}",
            ready.as_micros(),
            created.as_micros(),
            began.elapsed().as_micros()
        );
        let processes = tree(node.child.id());
        say_memory(1, "up", &processes);
        // Said once the box is deleted, so a refused reading leaves no VM running into the next run's measure.
        let usage = client.usage(&id);
        let deleting = Instant::now();
        client.delete(&id).unwrap();
        println!(
            "NODE-GUEST delete start=1 us={}",
            deleting.elapsed().as_micros()
        );
        drop(client);
        drop(node);
        ended(&processes.iter().map(|held| held.pid).collect::<Vec<_>>());
        let usage = usage.unwrap();
        println!(
            "NODE-GUEST box_usage start=1 memory_bytes={} memory_peak_bytes={} cpu_usec={}",
            usage.memory_bytes, usage.memory_peak_bytes, usage.cpu_usec
        );
    }
}
