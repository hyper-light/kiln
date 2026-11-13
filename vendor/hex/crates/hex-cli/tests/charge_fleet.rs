//! Joint plan step 14 (AC-T7.4.1, AC-T7.5.2; `docs/design/learned-charge.md`, "what ROOT measures"): density and fairness
//! of the learned charge on a real node, ROOT's run. A real `hex node` runs a fleet of harness VMs, each replaying a
//! recorded agent trace (its busy threads and the memory it holds over time, played by a shell in the guest), one of
//! them the riser, beside a process box under contention. The driver admits VMs as the scheduler's ledger books them,
//! each at its class's learned prior where the node's capacity answer carries one, else its whole receipt (C2, C6),
//! whenever the node's room has changed since it last refused one; every window (the node's learner's as the node says
//! it, each read at least that long after the last), it reads each VM's CPU given (its usage), asked (its trace's over
//! the same span) and booked (its holding on the capacity answer), and reckons the run as hex-sim reckons its own
//! (`hex_sim::charge::trial`). A VM whose trace has played is deleted, freeing its room. It prints `HEX-CHARGE-FLEET`
//! lines and keeps every reading in `HEX_CHARGE_OUT/windows.tsv`.
//!
//! What it measures, each from the run's own windows: the most VMs admitted at once against what envelope charging
//! admits; each VM-window's given over min(asked, booked), the guarantee's misses and Jain's index of the contended
//! windows; the riser's windows past its booking, its neighbours' and the process box's shortfalls while it rises, the
//! starts refused for room while it rises, and whether the last resort ended it (its inspect's why); each VM's windows
//! past its charge, the violation rate; and the learner's own CPU (Linux: the node's `hex-charges` thread; macOS and
//! Windows: the node's whole CPU, an upper bound, no thread named there).
//!
//! The riser rises once its charge has warmed, SEEDABLE windows after its start, to every vCPU and the most any trace
//! holds; the run lasts its longest trace. Each VM's guest plays its trace with its `/bin/sh` (busybox's `sleep`, `head`,
//! `truncate`, `grep` and `kill` in an Alpine root), each step until its deadline from the trace's start (read from
//! `/proc/uptime`, so the play doesn't drift from the trace), holding its memory in a file of the first of `/dev/shm`,
//! `/tmp` and `/run` that `/proc/mounts` says is a tmpfs, else of a tmpfs it mounts: guest RAM, never a file a shared
//! root would put on the host. Equal steps in a row are one; a trace whose player's arguments pass Linux's least
//! `ARG_MAX` (the guest's exec bound, `hex_api::sim::SIM_EXEC_LIMIT`) is refused, naming it.
//!
//! Every number it runs with is derived, none picked:
//! - each VM's workload, what it is admitted for and the class's cost sizes: the most busy threads (a CPU each) and the
//!   most MiB any step of any trace asks, so every trace and the riser fit their VM's envelope;
//! - the process box's CPU cap, which it books and asks whole: the median CPU (mean cores, in milli-CPU, nearest rank)
//!   of the `build` legs kiln's CI ran from 2026-10-22 to 2026-11-05 (bench/sched/kiln-ci-20261022-20261105.trace),
//!   a CI job's box beside the agents;
//! - the deadline guarding the node's start and each create (a VM's create boots it, past the client's learned create
//!   bound): twice the node's own boot bound for the class, so the node's refusal, typed, comes first. `shards`: the
//!   guest's dial bound, twice the slowest boot to ready this host's class records (bench/native/guest/records.tsv);
//!   `shards-image`: the image boot's, twice its `image_boot` record, refused, naming the record to take, until
//!   records.tsv holds one, as the class's creates are; `firecracker`: its configured `boot_ms`;
//! - the run's window: the node's learner's, as the node says it.
//!
//! Inputs, each by its own variable, a missing one failing by name:
//! - `HEX_CHARGE_CLASS`: `shards-image` (Agentfile image VMs of `HEX_IMAGE`, `NAME@sha256:...`, which the driver pulls
//!   into the node's Shards home first: Linux, its run host awaiting `RunHostStdioOnly` on macOS and Windows), `shards`
//!   (Shards guest VMs whose box plays the trace from `HEX_CHARGE_ROOTFS`) or `firecracker` (Linux, `HEX_CHARGE_ROOTFS`
//!   a root directory). The root: Alpine 3.24.4's minirootfs of the guest's architecture, pinned by its SHA-256 in
//!   bench/native/oci-validation/inputs.txt (fetched there by fetch.sh), unpacked to a directory, or the tarball itself
//!   where the node stages a root from one (Windows);
//! - the class's configuration as `hex node` reads it, passed through to it: `SHARDS_BIN`, `SHARDS_KERNEL` and
//!   `SHARDS_INIT` (the image class's run hosts), `HEX_GUEST_KERNEL`, `HEX_GUEST_INITRD`, `HEX_GUEST_CPUS`,
//!   `HEX_GUEST_MEMORY_MIB`, `HEX_GUEST_COST_FILE`, `HEX_GUEST_COST_SHA256`, `HEX_GUEST_COST_EVIDENCE`; Firecracker's
//!   `HEX_FIRECRACKER_CONFIG`;
//! - `HEX_CHARGE_TRACES`: a directory of recorded agent traces, a file each, a line each `millis threads mib` (so many
//!   milliseconds of so many busy threads holding so many MiB; `#` comments), as bench/charge/record_trace.py records
//!   a real harness session;
//! - `HEX_CHARGE_OUT`: a directory of the run's own, new or empty.
//!
//! Run alone, `--ignored`, from the repository's root:
//!   Linux: build first, then run the test binary alone in a cgroup delegated to it, cargo outside it (cargo in the
//!   scope's root holds it, and the node can't hand its controllers down: EBUSY):
//!     cargo test --release --locked -p hex-cli --test charge_fleet --no-run    (it says the binary's path)
//!     HEX_DELEGATED=1 systemd-run --user --scope -p Delegate=yes -- <that binary> --ignored --nocapture
//!   macOS and Windows:
//!     cargo test --release --locked -p hex-cli --test charge_fleet -- --ignored --nocapture
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    clippy::arithmetic_side_effects,
    clippy::disallowed_macros
)]

use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::fs;
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio as HostStdio};
use std::sync::mpsc;
use std::time::{Duration, Instant};

use hex_api::{
    Allocation, Bounds, BoxId, BoxSpec, BoxState, CapacityResources, Class, ClassSpec, Network,
    OciSpec, Program, Rootfs, Runtime, ShardsImage, Stdio,
};
use hex_client::Client;
use hex_sim::charge::SEEDABLE;
use hex_sim::charge::trial::{self, Cast, Reading, Window};

fn input(name: &str) -> String {
    std::env::var(name)
        .unwrap_or_else(|_| panic!("{name} is unset (crates/hex-cli/tests/charge_fleet.rs)"))
}

/// One step of a trace: so long, so many busy threads, so many MiB held.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Step {
    millis: u64,
    threads: u64,
    mib: u64,
}

/// The recorded traces in `dir`, by file name, each refused by name where a line isn't three numbers.
fn traces(dir: &Path) -> Vec<(String, Vec<Step>)> {
    let mut found = Vec::new();
    for entry in fs::read_dir(dir).unwrap() {
        let path = entry.unwrap().path();
        let text = fs::read_to_string(&path).unwrap();
        let steps: Vec<Step> = text
            .lines()
            .map(|line| line.split('#').next().unwrap_or("").trim())
            .filter(|line| !line.is_empty())
            .map(|line| {
                let words: Vec<u64> = line
                    .split_whitespace()
                    .map(|w| {
                        w.parse()
                            .unwrap_or_else(|e| panic!("{}: {line:?}: {e}", path.display()))
                    })
                    .collect();
                let [millis, threads, mib] = words[..] else {
                    panic!("{}: {line:?} isn't `millis threads mib`", path.display());
                };
                Step {
                    millis,
                    threads,
                    mib,
                }
            })
            .collect();
        assert!(!steps.is_empty(), "{} holds no step", path.display());
        found.push((
            path.file_name().unwrap().to_string_lossy().into_owned(),
            steps,
        ));
    }
    found.sort_by(|a, b| a.0.cmp(&b.0));
    assert!(!found.is_empty(), "{} holds no trace", dir.display());
    found
}

fn length(steps: &[Step]) -> Duration {
    Duration::from_millis(steps.iter().map(|s| s.millis).sum())
}

/// What a trace asks over `from..to` of its time, in milli-CPU: its busy threads, each a CPU, at most `vcpus` of them,
/// averaged over the span; nothing past its end.
fn asked_over(steps: &[Step], from: Duration, to: Duration, vcpus: u64) -> u64 {
    let span = to.saturating_sub(from).as_nanos();
    if span == 0 {
        return 0;
    }
    let (mut at, mut busy) = (Duration::ZERO, 0u128);
    for step in steps {
        let end = at + Duration::from_millis(step.millis);
        let overlap = end.min(to).saturating_sub(at.max(from));
        busy += overlap.as_nanos() * u128::from(step.threads.min(vcpus));
        at = end;
        if at >= to {
            break;
        }
    }
    u64::try_from(busy * 1000 / span).unwrap()
}

/// `usec` of CPU over `span`, in milli-CPU.
fn milli(usec: u64, span: Duration) -> u64 {
    u64::try_from(u128::from(usec) * 1000 / span.as_micros().max(1)).unwrap()
}

/// The guest's player, given `millis threads mib` triples: each step its threads busy and its MiB held, until the
/// step's deadline from the trace's start (`/proc/uptime`, in centiseconds), so a slow step doesn't push the next. Its
/// memory is held in a tmpfs file, grown and cut in place: `/dev/shm`, `/tmp` or `/run` where `/proc/mounts` says one is
/// a tmpfs, else one it mounts; with none, or no `/proc/uptime`, it says so and ends.
const PLAYER: &str = r#"fail() { echo "hex-charge player: $*" >&2; exit 2; }
[ -r /proc/uptime ] || fail "no /proc/uptime to keep the trace's time by"
hold=
for d in /dev/shm /tmp /run; do
  if grep -qs "^[^ ]* $d tmpfs " /proc/mounts; then hold=$d/hex-charge-hold; break; fi
done
if [ -z "$hold" ]; then
  mkdir -p /run/hex-charge && mount -t tmpfs tmpfs /run/hex-charge 2>/dev/null && hold=/run/hex-charge/hold ||
    fail "no tmpfs to hold memory in (/dev/shm, /tmp, /run), and none could be mounted"
fi
: > "$hold" || fail "can't write $hold"
cs() { read -r up _ < /proc/uptime; echo "${up%.*}${up#*.}"; }
start=$(cs); spent=0; held=0
spin() { while :; do :; done; }
while [ $# -ge 3 ]; do
  ms=$1; threads=$2; mib=$3; shift 3
  pids=
  i=0
  while [ "$i" -lt "$threads" ]; do spin & pids="$pids $!"; i=$((i + 1)); done
  if [ "$mib" -gt "$held" ]; then
    head -c "$(((mib - held) * 1048576))" /dev/zero >> "$hold" || fail "can't hold $mib MiB"
  elif [ "$mib" -lt "$held" ]; then
    truncate -s "$((mib * 1048576))" "$hold" || fail "can't let go to $mib MiB"
  fi
  held=$mib
  spent=$((spent + ms))
  left=$((start + spent / 10 - $(cs)))
  if [ "$left" -gt 0 ]; then sleep "$((left / 100)).$(printf %02d $((left % 100)))"; fi
  if [ -n "$pids" ]; then kill $pids 2>/dev/null; fi
  wait 2>/dev/null
done
rm -f "$hold"
"#;

/// `steps` with each run of equal steps (the same threads and MiB) one step of their whole time.
fn merged(steps: &[Step]) -> Vec<Step> {
    let mut out: Vec<Step> = Vec::new();
    for step in steps {
        match out.last_mut() {
            Some(last) if (last.threads, last.mib) == (step.threads, step.mib) => {
                last.millis += step.millis;
            }
            _ => out.push(*step),
        }
    }
    out
}

/// The player's command for `steps`, refused, naming `trace`, where it passes the guest's exec bound: Linux's least
/// `ARG_MAX`, whatever its stack limit (`hex_api::sim::SIM_EXEC_LIMIT`).
fn player(trace: &str, steps: &[Step]) -> Vec<String> {
    let mut args = vec![
        "/bin/sh".to_owned(),
        "-c".to_owned(),
        PLAYER.to_owned(),
        "player".to_owned(),
    ];
    for step in merged(steps) {
        args.push(step.millis.to_string());
        args.push(step.threads.to_string());
        args.push(step.mib.to_string());
    }
    let words: Vec<&str> = args.iter().map(String::as_str).collect();
    let limit = hex_api::sim::SIM_EXEC_LIMIT;
    let size = limit.size(&Program::of(&words));
    assert!(
        size <= limit.block,
        "{trace}: its player takes {size} bytes of arguments, past the guest's exec bound ({}, {}): record a \
         shorter session",
        limit.block,
        limit.source
    );
    args
}

/// Each VM's workload (what it is admitted for, which the class's cost sizes): the most busy threads (a CPU each) and
/// the most MiB any step of any of `traces` asks, so every trace and the riser fit their VM.
fn workload(traces: &[(String, Vec<Step>)]) -> CapacityResources {
    let steps = || traces.iter().flat_map(|(_, steps)| steps);
    let threads = steps().map(|s| s.threads).max().unwrap_or(0);
    let mib = steps().map(|s| s.mib).max().unwrap_or(0);
    assert!(threads > 0, "no step of the traces asks for a CPU");
    CapacityResources {
        cpu_milli: threads * 1000,
        mem_bytes: mib << 20,
        disk_bytes: 0,
    }
}

/// kiln's CI legs from 2026-10-22 to 2026-11-05, as hex-sim's trace (bench/sched/kiln_trace.py made it).
const CI_LEGS: &str = include_str!("../../../bench/sched/kiln-ci-20261022-20261105.trace");

/// The process box's CPU cap: the median, by nearest rank, of the CPU (mean cores, in milli-CPU) of [`CI_LEGS`]' `build`
/// legs, a CI job's box beside the agents.
fn box_cpu() -> u64 {
    let mut cpu: Vec<u64> = CI_LEGS
        .lines()
        .filter_map(|line| line.strip_prefix("leg,"))
        .map(|leg| leg.split(',').collect::<Vec<_>>())
        .filter(|fields| fields.get(5) == Some(&"build"))
        .map(|fields| fields[2].parse().unwrap())
        .collect();
    assert!(!cpu.is_empty(), "the CI trace holds no build leg");
    cpu.sort_unstable();
    cpu[cpu.len().div_ceil(2) - 1]
}

/// The deadline guarding the node's start and each create of `kind`: twice the node's own boot bound for the class, so
/// the node's refusal, typed, comes first (module docs).
fn guard(kind: Kind) -> Duration {
    let node = match kind {
        Kind::Shards => {
            2 * hex_runtime::guest::recorded_boot().unwrap_or_else(|why| panic!("{why}"))
        }
        Kind::Image => {
            2 * hex_runtime::guest::recorded("image_boot").unwrap_or_else(|why| panic!("{why}"))
        }
        Kind::Firecracker => firecracker_boot(),
    };
    2 * node
}

/// The Firecracker backend's configured boot bound (`HEX_FIRECRACKER_CONFIG`'s `boot_ms`).
#[cfg(target_os = "linux")]
fn firecracker_boot() -> Duration {
    let path = PathBuf::from(input("HEX_FIRECRACKER_CONFIG"));
    hex_runtime::firecracker::Config::from_file(&path)
        .unwrap_or_else(|why| panic!("HEX_FIRECRACKER_CONFIG: {why}"))
        .boot_timeout
}

#[cfg(not(target_os = "linux"))]
fn firecracker_boot() -> Duration {
    panic!("firecracker VMs run on Linux alone")
}

/// The VMs' class, as `HEX_CHARGE_CLASS` names it, and the node class its capacity is asked of.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Kind {
    Image,
    Shards,
    Firecracker,
}

impl Kind {
    fn named() -> Self {
        match input("HEX_CHARGE_CLASS").as_str() {
            "shards-image" => Self::Image,
            "shards" => Self::Shards,
            "firecracker" => Self::Firecracker,
            other => panic!("HEX_CHARGE_CLASS {other:?}: shards-image, shards or firecracker"),
        }
    }

    fn class(self) -> Class {
        match self {
            Self::Image | Self::Shards => Class::Shards,
            Self::Firecracker => Class::Firecracker,
        }
    }

    /// A VM playing `args`, admitted with `allocation` for the run's workload `workload`.
    fn spec(
        self,
        id: BoxId,
        args: &[String],
        allocation: Allocation,
        workload: CapacityResources,
    ) -> BoxSpec {
        let words: Vec<&str> = args.iter().map(String::as_str).collect();
        let oci = || {
            let root = PathBuf::from(input("HEX_CHARGE_ROOTFS"));
            Box::new(OciSpec {
                rootfs: if root.is_dir() {
                    Rootfs::Dir(root)
                } else {
                    Rootfs::Layers(vec![root])
                },
                init: Program::of(&words),
                mounts: Vec::new(),
                hostname: None,
                network: Network::Private,
                hosts: Vec::new(),
            })
        };
        let class = match self {
            Self::Image => ClassSpec::ShardsImage(Box::new(ShardsImage {
                command: Some(args.to_vec()),
                ..ShardsImage::new(&input("HEX_IMAGE"))
            })),
            Self::Shards => ClassSpec::Shards(oci()),
            Self::Firecracker => ClassSpec::Firecracker(oci()),
        };
        BoxSpec {
            id,
            bounds: Bounds {
                cpu_max_milli: Some(u32::try_from(workload.cpu_milli).unwrap()),
                memory_max: Some(workload.mem_bytes),
                ..Bounds::default()
            },
            class,
            owner: None,
            devices: Vec::new(),
            allocation: Some(allocation),
            route: None,
            band: 0,
        }
    }
}

/// Pulls `HEX_IMAGE` into the node's Shards home and cache (`hex-node`'s `shards_native_paths`: beside its records in
/// `state`), whose store its image VMs boot from, then ends the daemon the pull started there, so no process of the
/// pull's runs beside the node (as `image_node::pull` does for the image class's own tests).
fn pull_image(state: &Path) {
    let shards = PathBuf::from(input("SHARDS_BIN"));
    let image = input("HEX_IMAGE");
    for args in [vec!["pull", image.as_str()], vec!["daemon", "stop"]] {
        let status = Command::new(&shards)
            .args(&args)
            .env("SHARDS_HOME", state.join("shards-native-home"))
            .env("SHARDS_CACHE", state.join("shards-native-cache"))
            .stdin(HostStdio::null())
            .status()
            .unwrap();
        assert!(status.success(), "shards {args:?}: {status}");
    }
}

/// A `hex node` of the run's own, ended by its pid as it drops; what it said on stderr is kept in `stderr`.
struct Node {
    child: Child,
    socket: PathBuf,
    stderr: PathBuf,
}

impl Node {
    fn start(work: &Path, wait: Duration) -> Self {
        let socket = work.join("node.sock");
        let stderr = work.join("node.stderr");
        let mut command = Command::new(env!("CARGO_BIN_EXE_hex"));
        command
            .arg("node")
            .arg("--socket")
            .arg(&socket)
            .arg("--state")
            .arg(work.join("state"));
        #[cfg(target_os = "linux")]
        command
            .arg("--box-root")
            .arg(own_cgroup("driver").join("boxes"));
        let mut child = command
            .stdin(HostStdio::null())
            .stdout(HostStdio::piped())
            .stderr(fs::File::create(&stderr).unwrap())
            .spawn()
            .unwrap();
        let (tx, rx) = mpsc::channel();
        let stdout = child.stdout.take().unwrap();
        std::thread::spawn(move || {
            for line in BufReader::new(stdout).lines() {
                let Ok(line) = line else { break };
                if tx.send(line).is_err() {
                    break;
                }
            }
        });
        // The node's own "ready <socket>" line is the fact waited on; the run's deadline only guards it.
        let want = format!("ready {}", socket.display());
        let until = Instant::now() + wait;
        loop {
            match rx.recv_timeout(until.saturating_duration_since(Instant::now())) {
                Ok(line) if line == want => break,
                Ok(line) => eprintln!("node: {line}"),
                Err(e) => panic!(
                    "node not ready: {e}; stderr: {}",
                    fs::read_to_string(&stderr).unwrap_or_default()
                ),
            }
        }
        Self {
            child,
            socket,
            stderr,
        }
    }

    /// The window the node's charge learner reads over, as the node said it; the run is refused where its learner
    /// doesn't run (Windows until its window is measured: docs/design/learned-charge.md C1).
    fn window(&self) -> Duration {
        let said = fs::read_to_string(&self.stderr).unwrap();
        if let Some(why) = said
            .lines()
            .find_map(|l| l.strip_prefix("hex node: the charge learner doesn't run: "))
        {
            panic!("REFUSED: the node's charge learner doesn't run: {why}");
        }
        let ns = said
            .lines()
            .find_map(|l| l.strip_prefix("hex node: the charge learner's window is "))
            .and_then(|rest| rest.strip_suffix(" ns"))
            .unwrap_or_else(|| panic!("the node named no charge window; stderr: {said}"));
        Duration::from_nanos(ns.parse().unwrap())
    }
}

impl Drop for Node {
    fn drop(&mut self) {
        if self.child.try_wait().unwrap().is_none() {
            self.child.kill().unwrap();
        }
        let status = self.child.wait().unwrap();
        eprintln!("CHARGE node ended: {status}");
    }
}

/// This binary's own cgroup, made a parent: it moves into the leaf `leaf`, as native_firecracker_smoke does.
#[cfg(target_os = "linux")]
fn own_cgroup(leaf: &str) -> PathBuf {
    assert!(
        std::env::var_os("HEX_DELEGATED").is_some(),
        "run inside a cgroup delegated to this binary (HEX_DELEGATED)"
    );
    let line = fs::read_to_string("/proc/self/cgroup").unwrap();
    let own = Path::new("/sys/fs/cgroup").join(line.trim().strip_prefix("0::/").unwrap());
    let at = own.join(leaf);
    fs::create_dir_all(&at).unwrap();
    fs::write(at.join("cgroup.procs"), std::process::id().to_string()).unwrap();
    own
}

/// The learner's own CPU so far and what it is: Linux reads the node's `hex-charges` thread (its schedstat's run
/// time); macOS and Windows read the node's whole CPU, an upper bound.
fn learner_cpu(pid: u32) -> (Duration, &'static str) {
    #[cfg(target_os = "linux")]
    {
        for task in fs::read_dir(format!("/proc/{pid}/task")).unwrap() {
            let task = task.unwrap().path();
            if fs::read_to_string(task.join("comm"))
                .unwrap_or_default()
                .trim()
                == "hex-charges"
            {
                let ns: u64 = fs::read_to_string(task.join("schedstat"))
                    .unwrap()
                    .split_whitespace()
                    .next()
                    .unwrap()
                    .parse()
                    .unwrap();
                return (Duration::from_nanos(ns), "the hex-charges thread");
            }
        }
        (Duration::ZERO, "no hex-charges thread")
    }
    #[cfg(target_os = "macos")]
    {
        let out = Command::new("/bin/ps")
            .args(["-o", "cputime=", "-p", &pid.to_string()])
            .output()
            .unwrap();
        let text = String::from_utf8_lossy(&out.stdout);
        let mut secs = 0f64;
        for part in text.trim().split(':') {
            secs = secs * 60.0 + part.parse::<f64>().unwrap();
        }
        (
            Duration::from_secs_f64(secs),
            "the node's whole CPU, an upper bound",
        )
    }
    #[cfg(windows)]
    {
        let out = Command::new("powershell")
            .args([
                "-NoProfile",
                "-Command",
                &format!("(Get-Process -Id {pid}).TotalProcessorTime.TotalMilliseconds"),
            ])
            .output()
            .unwrap();
        let ms: f64 = String::from_utf8_lossy(&out.stdout).trim().parse().unwrap();
        (
            Duration::from_secs_f64(ms / 1000.0),
            "the node's whole CPU, an upper bound",
        )
    }
}

/// A process box beside the fleet, asking its whole cap: as many spinners as its cap holds CPUs.
fn spinner(cpu_milli: u64) -> Program {
    let threads = cpu_milli.div_ceil(1000);
    #[cfg(not(windows))]
    {
        let script = format!(
            "i=0; while [ $i -lt {threads} ]; do (while :; do :; done) & i=$((i + 1)); done; wait"
        );
        Program::of(&["/bin/sh", "-c", &script])
    }
    #[cfg(windows)]
    {
        let system = std::env::var("SystemRoot").unwrap();
        let cmd = format!("{system}\\System32\\cmd.exe");
        let mut line = String::new();
        for _ in 1..threads {
            line.push_str("start /b cmd /c \"for /l %i in (0,0,1) do rem\" & ");
        }
        line.push_str("for /l %i in (0,0,1) do rem");
        Program::of(&[cmd.as_str(), "/c", &line])
    }
}

/// One VM of the fleet: its number in admission order, which its readings carry, and its CPU at its last reading and
/// when it was read (none before its first: a window's given is what it used since the one before, over its own span).
struct Vm {
    n: u32,
    id: BoxId,
    trace: String,
    steps: Vec<Step>,
    receipt: Allocation,
    started: Instant,
    last: Option<(u64, Instant)>,
}

/// Each VM's holding on the class's capacity answer, by box: its booking and what it holds past it (C8).
fn holdings(capacity: &hex_api::Capacity) -> BTreeMap<String, CapacityResources> {
    capacity
        .learned
        .iter()
        .flat_map(|learned| &learned.holdings)
        .map(|h| (h.id.as_str().to_owned(), h.holding))
        .collect()
}

/// A new VM's receipt: `allocation` booked at its class's prior where the capacity answer carries one, as the
/// scheduler's ledger books it (C2).
fn booked(capacity: &hex_api::Capacity, allocation: Allocation) -> Allocation {
    capacity
        .learned
        .iter()
        .flat_map(|learned| &learned.priors)
        .find(|prior| prior.profile == allocation.cost.id && prior.ceiling == allocation.resources)
        .map_or(allocation, |prior| allocation.booked_at(prior.booking))
}

fn fits(room: CapacityResources, need: CapacityResources) -> bool {
    room.holds(need)
}

/// The process box's readings' VM number: none of the fleet's.
const BOX: u32 = u32::MAX;

/// Joint plan step 14 (AC-T7.4.1, AC-T7.5.2): density and fairness of the learned charge on this host, ROOT's run (the
/// module docs). Do run the fleet its traces describe for their longest trace's length; expect its figures printed and
/// its windows kept: a record, which C4's and C1's falsifiers are read from (a miss in a contended window; windows past
/// a charge more often than one in SEEDABLE), not a verdict of its own.
#[test]
#[ignore = "ROOT's run on a real host: VMs, a delegated cgroup on Linux, recorded traces (module docs)"]
fn the_learned_charge_holds_density_and_fairness_on_this_host() {
    hex_core::patience::measure_all();
    let kind = Kind::named();
    let class = kind.class();
    let wait = guard(kind);
    let out = PathBuf::from(input("HEX_CHARGE_OUT"));
    // A run's node starts from no records: one left by an earlier run would be recovered into this one.
    assert!(
        fs::read_dir(&out).map_or(true, |mut entries| entries.next().is_none()),
        "HEX_CHARGE_OUT {} must be new or empty",
        out.display()
    );
    fs::create_dir_all(&out).unwrap();
    let traces = traces(Path::new(&input("HEX_CHARGE_TRACES")));
    let workload = workload(&traces);
    if kind == Kind::Image {
        pull_image(&out.join("state"));
    }
    let node = Node::start(&out, wait);
    let window = node.window();
    let client = Client::connect(&node.socket).unwrap();
    let capacity = client
        .capacity_before(class, Instant::now() + wait)
        .unwrap()
        .expect("the class's capacity");
    let cost = capacity.cost.expect("the class's cost profile");
    let allocation = cost.allocate(workload).unwrap();
    let vcpus = allocation.guest().unwrap().cpu_milli.div_ceil(1000);
    let box_cpu = box_cpu();
    let budget = CapacityResources {
        cpu_milli: capacity.total.cpu_milli.saturating_sub(box_cpu),
        ..capacity.total
    };
    let envelope = [
        (budget.cpu_milli, allocation.resources.cpu_milli),
        (budget.mem_bytes, allocation.resources.mem_bytes),
    ]
    .into_iter()
    .filter(|(_, each)| *each > 0)
    .map(|(all, each)| all / each)
    .min()
    .unwrap();
    // The process box under contention: it books its caps and asks them whole.
    let boxed = BoxId::new("charge-box").unwrap();
    let _spinning = client
        .run_before(
            BoxSpec {
                id: boxed.clone(),
                bounds: Bounds {
                    cpu_max_milli: Some(u32::try_from(box_cpu).unwrap()),
                    memory_max: Some(hex_api::MIN_MEMORY),
                    ..Bounds::default()
                },
                class: ClassSpec::Process,
                owner: None,
                devices: Vec::new(),
                allocation: None,
                route: None,
                band: 0,
            },
            spinner(box_cpu),
            Stdio::null(),
            Instant::now() + wait,
        )
        .unwrap();
    let begun = Instant::now();
    let run = traces.iter().map(|(_, steps)| length(steps)).max().unwrap();
    let rises_after = window * u32::try_from(SEEDABLE).unwrap();
    assert!(
        rises_after < run,
        "the longest trace ({run:?}) is shorter than a riser's warm-up, SEEDABLE windows ({rises_after:?})"
    );
    let peak_mib = traces
        .iter()
        .flat_map(|(_, steps)| steps)
        .map(|s| s.mib)
        .max()
        .unwrap();
    let mut vms: Vec<Vm> = Vec::new();
    let mut windows: Vec<Window> = Vec::new();
    let mut tsv =
        String::from("window\tvm\ttrace\tspan_us\tasked_milli\tgiven_milli\tbooked_milli\n");
    let (mut admitted, mut refused, mut refused_rising, mut peak) = (0u64, 0u64, 0u64, 0usize);
    let mut refused_at: Option<CapacityResources> = None;
    let mut rise_window = None;
    let mut box_last: Option<(u64, Instant)> = None;
    let (mut played, mut gone_otherwise) = (0u64, 0u64);
    // Each window's readings come at least a window after the last's, however long an admission's create took: a
    // window is never a sliver caught up after a slow one.
    let mut next = Instant::now() + window;
    while begun.elapsed() < run {
        hex_wait::rest_until(next).unwrap();
        let capacity = client
            .capacity_before(class, Instant::now() + wait)
            .unwrap()
            .expect("the class's capacity");
        let held = holdings(&capacity);
        // Admission (C6): the next VM's booking against the budget less every VM's holding, its receipt where none.
        let taken = vms.iter().fold(CapacityResources::default(), |sum, vm| {
            sum.checked_add(
                held.get(vm.id.as_str())
                    .copied()
                    .unwrap_or(vm.receipt.booking()),
            )
            .unwrap()
        });
        let room = budget.checked_sub(taken).unwrap_or_default();
        let next_receipt = booked(&capacity, allocation);
        if refused_at != Some(room) {
            if fits(room, next_receipt.booking()) {
                let (trace, steps) = &traces[usize::try_from(admitted).unwrap() % traces.len()];
                let mut steps = steps.clone();
                if admitted == 0 {
                    // The riser: its trace until its charge has warmed, then every vCPU and the most any trace holds.
                    let mut kept = Vec::new();
                    let mut spent = 0u64;
                    let rise = u64::try_from(rises_after.as_millis()).unwrap();
                    for step in &steps {
                        if spent >= rise {
                            break;
                        }
                        let millis = step.millis.min(rise - spent);
                        kept.push(Step { millis, ..*step });
                        spent += millis;
                    }
                    kept.push(Step {
                        millis: u64::try_from(run.as_millis()).unwrap() - spent,
                        threads: vcpus,
                        mib: peak_mib,
                    });
                    steps = kept;
                }
                let n = u32::try_from(admitted).unwrap();
                let id = BoxId::new(&format!("charge-vm-{n}")).unwrap();
                let info = client
                    .create_before(
                        kind.spec(id.clone(), &player(trace, &steps), next_receipt, workload),
                        Instant::now() + wait,
                    )
                    .unwrap();
                if info.state == BoxState::Created {
                    client.start(&id).unwrap();
                }
                vms.push(Vm {
                    n,
                    id,
                    trace: trace.clone(),
                    steps,
                    receipt: next_receipt,
                    started: Instant::now(),
                    last: None,
                });
                admitted += 1;
                refused_at = None;
            } else {
                refused += 1;
                if rise_window.is_some() {
                    refused_rising += 1;
                }
                refused_at = Some(room);
            }
        }
        let at = windows.len();
        if rise_window.is_none()
            && vms
                .iter()
                .any(|vm| vm.n == 0 && vm.started.elapsed() >= rises_after)
        {
            rise_window = Some(at);
        }
        let mut readings = Vec::new();
        let mut ending = Vec::new();
        let running: BTreeMap<String, bool> = client
            .list()
            .unwrap()
            .into_iter()
            .map(|info| (info.id.as_str().to_owned(), info.state == BoxState::Running))
            .collect();
        for vm in &mut vms {
            // A VM whose trace has played ends with it: the driver deletes it, freeing its room. The riser's trace
            // lasts the run.
            if vm.started.elapsed() >= length(&vm.steps) {
                ending.push((vm.id.clone(), true));
                continue;
            }
            // A VM that ended otherwise (the last resort's end, or its player's own) is read no more.
            let usage = match running.get(vm.id.as_str()) {
                Some(true) => client.usage(&vm.id).ok(),
                _ => None,
            };
            let Some(usage) = usage else {
                ending.push((vm.id.clone(), false));
                continue;
            };
            let read = Instant::now();
            let Some((before, then)) = vm.last.replace((usage.cpu_usec, read)) else {
                continue;
            };
            let span = read.duration_since(then);
            let given = milli(usage.cpu_usec.saturating_sub(before), span);
            let asked = asked_over(
                &vm.steps,
                then.saturating_duration_since(vm.started),
                read.saturating_duration_since(vm.started),
                vcpus,
            );
            let booked = held
                .get(vm.id.as_str())
                .copied()
                .unwrap_or(vm.receipt.booking())
                .cpu_milli;
            let vm_n = vm.n;
            readings.push(Reading {
                vm: vm_n,
                asked,
                given,
                booked,
            });
            writeln!(
                tsv,
                "{at}\t{vm_n}\t{}\t{}\t{asked}\t{given}\t{booked}",
                vm.trace,
                span.as_micros()
            )
            .unwrap();
        }
        let usage = client.usage(&boxed).unwrap();
        let read = Instant::now();
        if let Some((before, then)) = box_last.replace((usage.cpu_usec, read)) {
            let span = read.duration_since(then);
            let given = milli(usage.cpu_usec.saturating_sub(before), span);
            readings.push(Reading {
                vm: BOX,
                asked: box_cpu,
                given,
                booked: box_cpu,
            });
            writeln!(
                tsv,
                "{at}\tbox\t-\t{}\t{box_cpu}\t{given}\t{box_cpu}",
                span.as_micros()
            )
            .unwrap();
        }
        for (id, trace_played) in ending {
            if trace_played {
                played += 1;
            } else {
                gone_otherwise += 1;
            }
            // The riser is kept to the end, for its inspect.
            if id.as_str() != "charge-vm-0"
                && let Err(why) = client.delete_before(&id, Instant::now() + wait)
            {
                eprintln!("CHARGE delete {id}: {why}");
            }
            vms.retain(|vm| vm.id != id);
        }
        peak = peak.max(vms.len());
        windows.push(Window {
            budget: capacity.total.cpu_milli,
            readings,
        });
        next = Instant::now() + window;
    }
    let ran = begun.elapsed();
    let (learner, measured) = learner_cpu(node.child.id());
    let riser = client.inspect(&BoxId::new("charge-vm-0").unwrap()).ok();
    for vm in &vms {
        let _ = client.delete_before(&vm.id, Instant::now() + wait);
    }
    let _ = client.delete_before(&BoxId::new("charge-vm-0").unwrap(), Instant::now() + wait);
    let _ = client.delete_before(&boxed, Instant::now() + wait);
    fs::write(out.join("windows.tsv"), tsv).unwrap();

    let cast = Cast {
        riser: rise_window.map(|at| (0, at)),
        boxed: Some(BOX),
    };
    let report = trial::report(&cast, &windows);
    let (mut past, mut counted) = (0u64, 0u64);
    for n in 0..u32::try_from(admitted).unwrap() {
        let uses: Vec<u64> = windows
            .iter()
            .filter_map(|w| w.readings.iter().find(|r| r.vm == n))
            .map(|r| r.given)
            .collect();
        let (p, c) = trial::violations(&uses).unwrap();
        past += p;
        counted += c;
    }
    let os = std::env::consts::OS;
    println!(
        "HEX-CHARGE-FLEET os={os} class={kind:?} window_ns={} windows={} ran_ms={} vms_peak={peak} \
         envelope_density={envelope} admitted={admitted} refused_for_room={refused} ended_with_their_traces={played} \
         ended_otherwise={gone_otherwise}",
        window.as_nanos(),
        report.windows,
        ran.as_millis()
    );
    println!(
        "HEX-CHARGE-FLEET inputs workload_cpu_milli={} workload_mem_bytes={} allocation={:?} box_cpu_milli={box_cpu} \
         guard_ms={} traces={}",
        workload.cpu_milli,
        workload.mem_bytes,
        allocation.resources,
        wait.as_millis(),
        traces.len()
    );
    println!(
        "HEX-CHARGE-FLEET fairness contended={} ratio_n={} ratio_least_per_mille={} ratio_p1={} ratio_p50={} \
         misses={} jain_least_ppm={} jain_mean_ppm={} box_short={}",
        report.contended,
        report.ratios.n,
        report.ratios.least,
        report.ratios.p1,
        report.ratios.p50,
        report.misses,
        report.jain_least_ppm,
        report.jain_mean_ppm,
        report.box_short
    );
    let ended = riser.as_ref().and_then(|info| info.last_resort.as_ref());
    println!(
        "HEX-CHARGE-FLEET riser rise_window={rise_window:?} windows={} over_booking={} given_of_asked_ppm={} \
         neighbours_short={} refused_while_rising={refused_rising} exit={:?} last_resort={ended:?}",
        report.riser_windows,
        report.riser_over,
        report.riser_given_ppm,
        report.neighbours_short,
        riser.as_ref().and_then(|info| info.exit)
    );
    println!(
        "HEX-CHARGE-FLEET violations os={os} past={past} counted={counted} per_million={}",
        u128::from(past) * 1_000_000 / u128::from(counted.max(1))
    );
    println!(
        "HEX-CHARGE-FLEET learner cpu_ns={} of_ran_ns={} measured={measured:?}",
        learner.as_nanos(),
        ran.as_nanos()
    );
}
