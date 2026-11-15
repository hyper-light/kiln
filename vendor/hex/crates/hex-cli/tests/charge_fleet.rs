//! Joint plan step 14 (AC-T7.4.1, AC-T7.5.2; `docs/design/learned-charge.md`, "what ROOT measures"): density and fairness
//! of the learned charge on a real node, ROOT's run. A real `hex node` runs a fleet of harness VMs, each replaying a
//! recorded agent trace (its busy threads and the memory it holds over time, played by a shell in the guest), one of
//! them the riser, beside a process box under contention. The driver admits VMs as the scheduler's ledger books them,
//! each at its class's learned prior where the node's capacity answer carries one (a cold class's its admission), else
//! its whole receipt (C2, C6), whenever the node's room has changed since it last refused one; every window (the node's
//! learner's as the node says it, each read at least that long after the last), it reads each VM's CPU given (its
//! usage), asked (what its trace asked for certain over the same span) and booked (its holding on the capacity answer),
//! and reckons the run as hex-sim reckons its own (`hex_sim::charge::trial`). A VM whose trace has played is deleted,
//! freeing its room. It prints `HEX-CHARGE-FLEET` lines and keeps every reading in `HEX_CHARGE_OUT/windows.tsv`.
//!
//! What it measures, each from the run's own windows: the most VMs admitted at once against what envelope charging
//! admits; each VM-window's given over min(asked, booked), the guarantee's misses and Jain's index of the contended
//! windows, and the same ratio of given over asked where nothing contended, the run's own floor; the riser's windows
//! past its booking, its neighbours' and the process box's shortfalls while it rises, the starts refused for room while
//! it rises, and whether the last resort ended it (its inspect's why); each VM that ended before its trace had played,
//! by name, with its inspect's exit and why; each VM's windows past its charge, the violation rate; and the learner's
//! own CPU (Linux: the node's `hex-charges` thread; macOS and Windows: the node's whole CPU, an upper bound, no thread
//! named there).
//!
//! The riser plays the longest trace, which its warm-up fits: it rises once its charge has warmed, SEEDABLE windows
//! after its start, to every vCPU and the most any trace holds; the run lasts its longest trace. Each VM's guest plays
//! its trace with its `/bin/sh` (busybox's `sleep`, `head`, `truncate`, `grep`, `mount` and `kill` in an Alpine root),
//! the trace's time from its player's own start (`/proc/self/stat`'s start time, on `/proc/uptime`'s clock and tick,
//! proc(5)), each step until its deadline in that time, so the play keeps to the trace (a step already past is passed
//! over). Its CPU: a spinner a vCPU its trace uses, made before it plays and held stopped, each step continuing as many
//! as the step's threads and stopping the rest (a signal, so a step's edge waits on no fork). Its memory, played beside
//! its CPU by the same deadlines, so no step's hold makes the next step's CPU late: a file of a tmpfs it mounts, as
//! large as its trace's most MiB, else of the first of `/dev/shm`, `/tmp` and `/run` that `/proc/mounts` says is a
//! tmpfs: guest RAM, never a file a shared root would put on the host. Equal steps in a row are one; a trace whose
//! player's arguments pass Linux's least `ARG_MAX` (the guest's exec bound, `hex_api::sim::SIM_EXEC_LIMIT`) is refused,
//! naming it.
//!
//! What a VM asked for certain ([`Asked`]): no clock of the driver's reads the guest's, so a window's ask is the least
//! its trace can have asked over the span its reading can't have missed. The trace's zero, its player's start, came
//! within the VM's start: after its create was sent, before its start's answer, which every class gives once its
//! program has started (a start past its W* answering Pending is followed to its run, its window ending where the
//! driver first reads it running). A step's edge comes from two ticks of `/proc/uptime` (hundredths of a second) before
//! its time, its deadline and the trace's zero each read rounded down to a tick, to a tick after it, the player's sleep
//! reckoned from a clock read rounded down. What the guest's own turns add past that (a sleep's overshoot, the shell's
//! turn) no reading places: the run's floor, given over asked where nothing contended, says how far it went. A
//! reading's span runs from the last read's answer to this read's sending, where each usage was read somewhere between
//! its call's sending and its answer; its given is its usage over the span between the two answers, and its ask and
//! booking are what the trace and its holding certainly asked over the shorter span, over that one. A VM's first
//! reading spans its player's own start, its spinners' making, which no clock of the driver's places: it is given, and
//! owed nothing.
//!
//! Every number it runs with is derived, none picked:
//! - each VM's workload, what it is admitted for and the class's cost sizes: the most busy threads (a CPU each, at most
//!   the host's CPUs, all a VM is given: D191) and the most MiB any step of any trace asks, so every trace and the
//!   riser fit their VM's envelope;
//! - the process box's CPU cap and memory bound, which it books (the run's admission leaves it both) and of which it
//!   asks the CPU whole: the median CPU (mean cores, in milli-CPU) and the median peak memory, each by nearest rank, of
//!   the `build` legs kiln's CI ran from 2026-10-22 to 2026-11-05 (bench/sched/kiln-ci-20261022-20261105.trace), a CI
//!   job's box beside the agents. macOS holds a process box at no CPU cap (refused, typed): there the box books its
//!   memory alone and its spinners ask a CPU each, taking what the scheduler gives them;
//! - no wait of its own is a timer (the person's rule, 2026-11-13, and Shards' R03 for test waits): each call waits
//!   for its answer while the node's connection stands, a wait past its W* telling its run-out and following its party
//!   (decision 9), and the node's start waits for its ready line while the node runs, its end the refusal; the node's
//!   own waits refuse, typed, as theirs do. No boot record is read;
//! - the run's window: the node's learner's, as the node says it at its start, or as the class's capacity answers tell
//!   it as the caps it holds move;
//! - what a VM asked for certain: the tick of `/proc/uptime` and `/proc/self/stat` and the instants of the run's own
//!   calls;
//! - the riser's warm-up, SEEDABLE windows, and its player's tmpfs, its trace's most MiB.
//!
//! Inputs, each by its own variable, a missing one failing by name:
//! - `HEX_CHARGE_CLASS`: `shards-image` (Agentfile image VMs, on Linux, macOS and Windows), `shards` (Shards guest VMs
//!   whose box plays the trace from `HEX_CHARGE_ROOTFS`) or `firecracker` (Linux, `HEX_CHARGE_ROOTFS` a root directory).
//!   The root: Alpine 3.24.4's minirootfs of the guest's architecture, pinned by its SHA-256 in
//!   bench/native/oci-validation/inputs.txt (fetched there by fetch.sh), unpacked to a directory, or the tarball itself
//!   where the node stages a root from one (Windows);
//! - the image VMs' image, put in the node's Shards home before the node starts, as image_real takes it: either
//!   `HEX_IMAGE_CONTEXT`, an Agentfile image's build context, built there and pinned by the manifest digest its build
//!   says (no registry need serve it: `crates/hex-cli/tests/image_harness`, the in-tree harness image any arch builds,
//!   has the Alpine base's `/bin/sh` and busybox), or else `HEX_IMAGE`, `NAME@sha256:...`, pulled there by its digest;
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
    Allocation, Bounds, BoxId, BoxSpec, BoxState, CapacityResources, Class, ClassSpec, Exit,
    LifecycleCause, Network, OciSpec, Program, Refusal, Rootfs, Runtime, ShardsImage, Stdio,
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

/// A tick of the guest's clock as its player reads it: hundredths of a second, `/proc/uptime`'s resolution and the unit
/// of `/proc/self/stat`'s start time (USER_HZ, 100 on every Linux ABI; proc(5)).
const UPTIME_TICK: Duration = Duration::from_millis(10);

/// Nanoseconds, as the driver reckons instants and a trace's time: signed, a span before a trace's start reaching
/// below its zero.
fn ns(span: Duration) -> i128 {
    i128::try_from(span.as_nanos()).unwrap()
}

/// What a VM's trace asked at the least (module docs, "what a VM asked for certain"): at each moment of its time, the
/// fewest threads of any step its player may be playing then, a step's edge coming from two ticks of `/proc/uptime`
/// before its time to a tick after it (none before the trace's start or after its end), each step's threads at most
/// `vcpus`; kept as its breakpoints, the threads from each to the next, and the busy thread-time before each, so a
/// span's least is read in the log of its steps.
struct Asked {
    at: Vec<i128>,
    threads: Vec<u64>,
    busy: Vec<u128>,
}

impl Asked {
    fn of(steps: &[Step], vcpus: u64) -> Self {
        // Each step may be playing from two ticks before its start, its deadline and the trace's zero each read rounded
        // down, to a tick past its end, the player's sleep reckoned from a clock read rounded down; before its first step
        // and after its last, nothing plays.
        let (early, late) = (2 * ns(UPTIME_TICK), ns(UPTIME_TICK));
        let mut edges: Vec<(i128, bool, u64)> = Vec::new();
        let mut start = 0i128;
        for step in steps {
            let end = start + i128::from(step.millis) * 1_000_000;
            let threads = step.threads.min(vcpus);
            edges.push((start - early, true, threads));
            edges.push((end + late, false, threads));
            start = end;
        }
        edges.push((late, false, 0));
        edges.push((start - early, true, 0));
        edges.sort_by_key(|(at, _, _)| *at);
        let mut playing: BTreeMap<u64, u64> = BTreeMap::from([(0, 1)]);
        let (mut at, mut threads, mut busy) = (Vec::new(), Vec::new(), Vec::new());
        let mut sum = 0u128;
        let mut i = 0;
        while i < edges.len() {
            let here = edges[i].0;
            while i < edges.len() && edges[i].0 == here {
                let (_, begins, n) = edges[i];
                if begins {
                    *playing.entry(n).or_insert(0) += 1;
                } else if let Some(count) = playing.get_mut(&n) {
                    *count -= 1;
                    if *count == 0 {
                        playing.remove(&n);
                    }
                }
                i += 1;
            }
            if let (Some(&last_at), Some(&last_threads)) = (at.last(), threads.last()) {
                sum += u128::try_from(here - last_at).unwrap() * u128::from(last_threads);
            }
            at.push(here);
            threads.push(playing.keys().next().copied().unwrap_or(0));
            busy.push(sum);
        }
        Self { at, threads, busy }
    }

    /// The least busy thread-time up to `t` of the trace's time.
    fn until(&self, t: i128) -> u128 {
        let after = self.at.partition_point(|&at| at <= t);
        if after == 0 {
            return 0;
        }
        let i = after - 1;
        self.busy[i] + u128::try_from(t - self.at[i]).unwrap() * u128::from(self.threads[i])
    }

    /// The least busy thread-time over `len` of the trace's time from any instant of `from_lo..=from_hi`: the least of a
    /// sum piecewise linear in where it starts, so at an end of the range or where its start or its end meets a
    /// breakpoint.
    fn least(&self, from_lo: i128, from_hi: i128, len: i128) -> u128 {
        let over = |from: i128| self.until(from + len) - self.until(from);
        let mut least = over(from_lo).min(over(from_hi));
        for shift in [0, len] {
            let lo = self.at.partition_point(|&at| at < from_lo + shift);
            let hi = self.at.partition_point(|&at| at <= from_hi + shift);
            for &at in &self.at[lo..hi] {
                least = least.min(over(at - shift));
            }
        }
        least
    }
}

/// `usec` of CPU over `span`, in milli-CPU.
fn milli(usec: u64, span: Duration) -> u64 {
    u64::try_from(u128::from(usec) * 1000 / span.as_micros().max(1)).unwrap()
}

/// So much of `milli` milli-CPU as `part` is of `whole`: what a rate held over the span a reading is certain of comes
/// to over the span its given is reckoned over.
fn scaled(milli: u64, part: i128, whole: i128) -> u64 {
    if part <= 0 || whole <= 0 {
        return 0;
    }
    u64::try_from(
        u128::from(milli) * u128::try_from(part).unwrap() / u128::try_from(whole).unwrap(),
    )
    .unwrap()
}

/// The guest's player, given its spinners' number, its trace's most MiB, then `millis threads mib` triples (module
/// docs): the trace's time from its own start (`/proc/self/stat`'s start time, its 22nd field, in ticks since boot on
/// `/proc/uptime`'s clock), each step until its deadline in that time, a step whose end is past passed over. Its CPU:
/// its spinners, made and held stopped, each step continuing as many as its threads and stopping the rest. Its memory,
/// played beside by the same deadlines: a file grown and cut in place in a tmpfs it mounts as large as the most MiB,
/// else in `/dev/shm`, `/tmp` or `/run` where `/proc/mounts` says one is a tmpfs. Where it can't, it says why and ends
/// (its memory's play, failing, ends its CPU's by a signal).
const PLAYER: &str = r#"fail() { echo "hex-charge player: $*" >&2; exit 2; }
[ -r /proc/uptime ] || fail "no /proc/uptime to keep the trace's time by"
now() { read -r up _ < /proc/uptime; ticks=$((${up%.*} * 100 + 1${up#*.} - 100)); }
read -r stat < /proc/self/stat || fail "no /proc/self/stat to start the trace's time from"
n=0; base=
for f in ${stat##*)}; do
  n=$((n + 1))
  if [ "$n" -eq 20 ]; then base=$f; break; fi
done
[ -n "$base" ] || fail "no start time in /proc/self/stat: $stat"
most=$1; peak=$2; shift 2
hold=
if mkdir -p /run/hex-charge && mount -t tmpfs -o "size=${peak}m" hex-charge /run/hex-charge 2>/dev/null; then
  hold=/run/hex-charge/hold
else
  for d in /dev/shm /tmp /run; do
    if grep -qs "^[^ ]* $d tmpfs " /proc/mounts; then hold=$d/hex-charge-hold; break; fi
  done
fi
[ -n "$hold" ] || fail "no tmpfs to hold memory in: none of $peak MiB could be mounted, and /dev/shm, /tmp and /run are none"
: > "$hold" || fail "can't write $hold"
lost() { echo "hex-charge player: $*" >&2; kill -TERM $$; exit 2; }
memory() {
  spent=0; held=0
  while [ $# -ge 3 ]; do
    spent=$((spent + $1)); mib=$3; shift 3
    end=$((base + spent / 10))
    now
    [ "$ticks" -lt "$end" ] || continue
    if [ "$mib" -gt "$held" ]; then
      head -c "$(((mib - held) * 1048576))" /dev/zero >> "$hold" || lost "can't hold $mib MiB in $hold"
    elif [ "$mib" -lt "$held" ]; then
      truncate -s "$((mib * 1048576))" "$hold" || lost "can't let go to $mib MiB in $hold"
    fi
    held=$mib
    now
    left=$((end - ticks))
    if [ "$left" -gt 0 ]; then sleep "$((left / 100)).$(printf %02d $((left % 100)))"; fi
  done
}
spin() { while :; do :; done; }
pool=; i=0
while [ "$i" -lt "$most" ]; do
  spin &
  pool="$pool $!"
  kill -STOP $! || fail "can't hold a spinner stopped"
  i=$((i + 1))
done
trap 'if [ -n "$pool" ]; then kill -KILL $pool; fi; exit 2' TERM
memory "$@" &
spent=0
while [ $# -ge 3 ]; do
  spent=$((spent + $1)); threads=$2; shift 3
  end=$((base + spent / 10))
  now
  [ "$ticks" -lt "$end" ] || continue
  run=; rest=; i=0
  for p in $pool; do
    if [ "$i" -lt "$threads" ]; then run="$run $p"; else rest="$rest $p"; fi
    i=$((i + 1))
  done
  if [ -n "$rest" ]; then kill -STOP $rest; fi
  if [ -n "$run" ]; then kill -CONT $run; fi
  now
  left=$((end - ticks))
  if [ "$left" -gt 0 ]; then sleep "$((left / 100)).$(printf %02d $((left % 100)))"; fi
done
if [ -n "$pool" ]; then kill -KILL $pool; fi
wait
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

/// The player's command for `steps`, its spinners as many as its most threads up to `vcpus` and its tmpfs as large as
/// its most MiB, refused, naming `trace`, where it passes the guest's exec bound: Linux's least `ARG_MAX`, whatever its
/// stack limit (`hex_api::sim::SIM_EXEC_LIMIT`).
fn player(trace: &str, steps: &[Step], vcpus: u64) -> Vec<String> {
    let most = steps
        .iter()
        .map(|s| s.threads)
        .max()
        .unwrap_or(0)
        .min(vcpus);
    let peak = steps.iter().map(|s| s.mib).max().unwrap_or(0);
    let mut args = vec![
        "/bin/sh".to_owned(),
        "-c".to_owned(),
        PLAYER.to_owned(),
        "player".to_owned(),
        most.to_string(),
        peak.to_string(),
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

/// Each VM's workload (what it is admitted for, which the class's cost sizes): the most busy threads (a CPU each, at
/// most `host_cpus`, every CPU a VM is given, D191) and the most MiB any step of any of `traces` asks, so every trace
/// and the riser fit their VM.
fn workload(traces: &[(String, Vec<Step>)], host_cpus: u64) -> CapacityResources {
    let steps = || traces.iter().flat_map(|(_, steps)| steps);
    let threads = steps().map(|s| s.threads).max().unwrap_or(0).min(host_cpus);
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

/// The process box's caps, a CI job's box beside the agents: the median, each by nearest rank, of the CPU (mean cores,
/// in milli-CPU) and of the peak memory (bytes) of [`CI_LEGS`]' `build` legs (bench/sched/kiln_trace.py's
/// `leg,arrival,runs_for,cpu,mem,gpus,kind`).
fn ci_box() -> CapacityResources {
    let legs: Vec<(u64, u64)> = CI_LEGS
        .lines()
        .filter_map(|line| line.strip_prefix("leg,"))
        .map(|leg| leg.split(',').collect::<Vec<_>>())
        .filter(|fields| fields.get(5) == Some(&"build"))
        .map(|fields| (fields[2].parse().unwrap(), fields[3].parse().unwrap()))
        .collect();
    assert!(!legs.is_empty(), "the CI trace holds no build leg");
    let median = |mut values: Vec<u64>| {
        values.sort_unstable();
        values[values.len().div_ceil(2) - 1]
    };
    CapacityResources {
        cpu_milli: median(legs.iter().map(|(cpu, _)| *cpu).collect()),
        mem_bytes: median(legs.iter().map(|(_, mem)| *mem).collect()),
        disk_bytes: 0,
    }
}

/// What the process box books of its caps `caps`, the bounds its node holds (a native box books its caps, C4): both on
/// Linux and Windows; on macOS, which holds a process box at no CPU cap (refused, typed), its memory alone.
fn box_booking(caps: CapacityResources) -> CapacityResources {
    if cfg!(target_os = "macos") {
        CapacityResources {
            cpu_milli: 0,
            ..caps
        }
    } else {
        caps
    }
}

/// The process box beside the fleet, `id`, bounded at its booking `booked`: a CPU cap where it books CPU.
fn box_spec(id: BoxId, booked: CapacityResources) -> BoxSpec {
    BoxSpec {
        id,
        bounds: Bounds {
            cpu_max_milli: (booked.cpu_milli > 0).then(|| u32::try_from(booked.cpu_milli).unwrap()),
            memory_max: Some(booked.mem_bytes),
            ..Bounds::default()
        },
        class: ClassSpec::Process,
        owner: None,
        devices: Vec::new(),
        allocation: None,
        route: None,
        band: 0,
    }
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

    /// A VM playing `args`, admitted with `allocation` for the run's workload `workload`; an image VM of `image`.
    fn spec(
        self,
        id: BoxId,
        args: &[String],
        allocation: Allocation,
        workload: CapacityResources,
        image: Option<&str>,
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
                ..ShardsImage::new(
                    image.expect("the image VMs' image, put in the node's Shards home"),
                )
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

// The image class's real-node helpers, of which the driver takes its image's build and pull alone.
#[allow(dead_code)]
#[path = "image_node/mod.rs"]
mod image_node;

/// The name an image built from `HEX_IMAGE_CONTEXT` is tagged with in the node's Shards home; its VMs pin it by its
/// digest.
const BUILT: &str = "hex-charge-fleet/harness";

/// The image VMs' image, put in the node's Shards home before the node starts, as image_real takes it: built from
/// `HEX_IMAGE_CONTEXT`, an Agentfile image's build context, and pinned by the manifest digest its build says
/// (`image_node::built`); or else `HEX_IMAGE`, pulled there by its digest (`image_node::pull`). Either way the daemon it
/// started there is ended, so no process of it runs beside the node.
fn put_image(out: &Path) -> String {
    let shards = PathBuf::from(input("SHARDS_BIN"));
    let image = match std::env::var_os("HEX_IMAGE_CONTEXT") {
        Some(context) => {
            assert!(
                std::env::var_os("HEX_IMAGE").is_none(),
                "HEX_IMAGE_CONTEXT and HEX_IMAGE are both set: the image comes from one"
            );
            image_node::built(&shards, Path::new(&context), out, BUILT)
        }
        None => {
            let image = input("HEX_IMAGE");
            image_node::pull(&shards, &image, &out.join("state"));
            image
        }
    };
    eprintln!("CHARGE image {image}");
    image
}

/// A `hex node` of the run's own, ended by its pid as it drops, on Windows with its keeper (image_node's
/// `end_keeper`); what it said on stderr is kept in `stderr`.
struct Node {
    child: Child,
    socket: PathBuf,
    stderr: PathBuf,
    /// The node's keeper's folder (`state\keeper`, hex-cli's `node.rs`), whose `keeper.owner` names it.
    #[cfg(windows)]
    keeping: PathBuf,
}

impl Node {
    fn start(work: &Path) -> Self {
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
        // Its stdout is read for its whole life, every line after the wait shown, so the node never writes to a pipe no
        // one reads (as image_node's).
        let (tx, rx) = mpsc::channel();
        let stdout = child.stdout.take().unwrap();
        std::thread::spawn(move || {
            for line in BufReader::new(stdout).lines() {
                let Ok(line) = line else { break };
                if let Err(mpsc::SendError(line)) = tx.send(line) {
                    eprintln!("node: {line}");
                }
            }
        });
        // The node's own "ready <socket>" line is the fact waited on, as long as the node runs: its stdout's end, with
        // it, is the refusal.
        let want = format!("ready {}", socket.display());
        loop {
            match rx.recv() {
                Ok(line) if line == want => break,
                Ok(line) => eprintln!("node: {line}"),
                Err(e) => panic!(
                    "node ended before it was ready: {e}; stderr: {}",
                    fs::read_to_string(&stderr).unwrap_or_default()
                ),
            }
        }
        Self {
            child,
            socket,
            stderr,
            #[cfg(windows)]
            keeping: work.join("state").join("keeper"),
        }
    }

    /// The window the node's charge learner reads its VMs over as the node said it at its start: Linux's, `cpu.max`'s
    /// period; on macOS and Windows none until it holds a cap, the longest period of the caps it holds then being told on
    /// the class's capacity answers (docs/design/learned-charge.md C1). The run is refused where its learner doesn't run.
    fn window(&self) -> Option<Duration> {
        let said = fs::read_to_string(&self.stderr).unwrap();
        if let Some(why) = said
            .lines()
            .find_map(|l| l.strip_prefix("hex node: the charge learner doesn't run: "))
        {
            panic!("REFUSED: the node's charge learner doesn't run: {why}");
        }
        let window = said
            .lines()
            .find_map(|l| l.strip_prefix("hex node: the charge learner's window is "))
            .unwrap_or_else(|| panic!("the node named no charge window; stderr: {said}"));
        window
            .strip_suffix(" ns")
            .map(|ns| Duration::from_nanos(ns.parse().unwrap()))
    }
}

impl Drop for Node {
    fn drop(&mut self) {
        if self.child.try_wait().unwrap().is_none() {
            self.child.kill().unwrap();
        }
        let status = self.child.wait().unwrap();
        eprintln!("CHARGE node ended: {status}");
        #[cfg(windows)]
        image_node::end_keeper(&self.keeping);
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

/// One usage read: what the box had used, in µs, and when its call was sent and answered (its usage was read somewhere
/// between).
#[derive(Debug, Clone, Copy)]
struct Read {
    cpu_usec: u64,
    sent: Instant,
    answered: Instant,
}

/// What a reading between `last` and `this` reckons over (module docs, "what a VM asked for certain"): its given's span,
/// between the two reads' answers, and the span within it the reading is certain of, from the last read's answer to
/// this read's sending (none where they cross), each in ns.
fn spans(last: &Read, this: &Read) -> (i128, i128) {
    (
        ns(this.answered.duration_since(last.answered)),
        ns(this.sent.saturating_duration_since(last.answered)),
    )
}

/// One VM of the fleet: its number in admission order, which its readings carry; its trace, and what it asked for
/// certain; when its create was sent (its trace's zero came after) and when it started (came before: its start's
/// answer, or where its start answered Pending the read that first found it running; none while it boots); and its
/// last read and how many readings it has given (none before its second read: a window's given is what it used since
/// the one before, over its own span).
struct Vm {
    n: u32,
    id: BoxId,
    trace: String,
    steps: Vec<Step>,
    asked: Asked,
    receipt: Allocation,
    from: Instant,
    started: Option<Instant>,
    last: Option<Read>,
    readings: u64,
}

impl Vm {
    /// What it asked for certain over the reading between `last` and `this`, in milli-CPU over its given's span: none
    /// for its first, which spans its player's own start.
    fn asked_for_certain(&self, started: Instant, last: &Read, this: &Read) -> u64 {
        let (nominal, certain) = spans(last, this);
        if self.readings == 0 || certain <= 0 {
            return 0;
        }
        // The trace's time at the span's start: its zero came after `from` and before `started`.
        let least = self.asked.least(
            ns(last.answered.duration_since(started)),
            ns(last.answered.duration_since(self.from)),
            certain,
        );
        u64::try_from(least * 1000 / u128::try_from(nominal.max(1)).unwrap()).unwrap()
    }
}

/// The learner's first window over the fleet's first VM, told on the class's capacity answer (C1): a fact no recorded
/// operation makes, so its wait measures (hex_testkit::waits).
static TOLD_WINDOW: hex_core::patience::Patience =
    hex_core::patience::Patience::measuring("test.hex_cli.charge_fleet.told_window");

/// Each VM's holding on the class's capacity answer, by box: its booking and what it holds past it (C8).
fn holdings(capacity: &hex_api::Capacity) -> BTreeMap<String, CapacityResources> {
    capacity
        .learned
        .iter()
        .flat_map(|learned| &learned.holdings)
        .filter_map(|h| match &h.id {
            hex_api::LearnedHolder::Box(id) => Some((id.as_str().to_owned(), h.holding)),
            // A node VM's own: no box of the fleet's.
            hex_api::LearnedHolder::Node(_) => None,
        })
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
    // Each call waits for its answer while the node's connection stands: past its W*, its run-out told and its party
    // followed (decision 9).
    let kind = Kind::named();
    let class = kind.class();
    let out = PathBuf::from(input("HEX_CHARGE_OUT"));
    // A run's node starts from no records: one left by an earlier run would be recovered into this one.
    assert!(
        fs::read_dir(&out).map_or(true, |mut entries| entries.next().is_none()),
        "HEX_CHARGE_OUT {} must be new or empty",
        out.display()
    );
    fs::create_dir_all(&out).unwrap();
    let traces = traces(Path::new(&input("HEX_CHARGE_TRACES")));
    let image = (kind == Kind::Image).then(|| put_image(&out));
    let node = Node::start(&out);
    let mut window = node.window();
    // The riser: the first VM admitted where the node told its window at its start; else the second, once its first
    // VM's cap has given its learner a window to warm the riser's charge over.
    let riser: u32 = if window.is_some() { 0 } else { 1 };
    let client = Client::connect(&node.socket).unwrap();
    let capacity = client
        .capacity(class)
        .unwrap()
        .expect("the class's capacity");
    let workload = workload(&traces, capacity.total.cpu_milli / 1000);
    let cost = capacity.cost.expect("the class's cost profile");
    let allocation = cost.allocate(workload).unwrap();
    let vcpus = allocation.guest().unwrap().cpu_milli.div_ceil(1000);
    // The fleet's budget: the machine less the process box's caps, which it books (a native box's booking is its caps,
    // as the node's learner reads them, C4).
    let box_caps = ci_box();
    let box_booked = box_booking(box_caps);
    // What the box's spinners ask: its CPU cap, where its node holds one; else a CPU each spinner.
    let box_cpu = if box_booked.cpu_milli > 0 {
        box_booked.cpu_milli
    } else {
        box_caps.cpu_milli.div_ceil(1000) * 1000
    };
    let budget = capacity
        .total
        .checked_sub(box_booked)
        .expect("the machine holds the process box's booking");
    let envelope = [
        (budget.cpu_milli, allocation.resources.cpu_milli),
        (budget.mem_bytes, allocation.resources.mem_bytes),
    ]
    .into_iter()
    .filter(|(_, each)| *each > 0)
    .map(|(all, each)| all / each)
    .min()
    .unwrap();
    // The process box under contention: it books its caps and asks its CPU whole.
    let boxed = BoxId::new("charge-box").unwrap();
    let _spinning = client
        .run(
            box_spec(boxed.clone(), box_booked),
            spinner(box_caps.cpu_milli),
            Stdio::null(),
        )
        .unwrap();
    let begun = Instant::now();
    // The riser's trace, the longest: the run's length, which its warm-up must fit.
    let longest = traces
        .iter()
        .max_by_key(|(_, steps)| length(steps))
        .unwrap();
    let run = length(&longest.1);
    let warm_up = |window: Duration| {
        let rises_after = window * u32::try_from(SEEDABLE).unwrap();
        assert!(
            rises_after < run,
            "the longest trace ({run:?}) is shorter than a riser's warm-up, SEEDABLE windows ({rises_after:?})"
        );
        rises_after
    };
    if let Some(window) = window {
        warm_up(window);
    }
    let peak_mib = traces
        .iter()
        .flat_map(|(_, steps)| steps)
        .map(|s| s.mib)
        .max()
        .unwrap();
    let mut vms: Vec<Vm> = Vec::new();
    let mut windows: Vec<Window> = Vec::new();
    let mut tsv = String::from(
        "window\tvm\ttrace\tspan_us\tcertain_us\tasked_milli\tgiven_milli\tbooked_milli\n",
    );
    let (mut admitted, mut refused, mut refused_rising, mut peak) = (0u64, 0u64, 0u64, 0usize);
    let mut refused_at: Option<CapacityResources> = None;
    let mut rise_window = None;
    // When the riser rises in its trace's time, placed at its admission (the window it warms over then).
    let mut rises_after: Option<Duration> = None;
    let mut box_last: Option<Read> = None;
    let (mut played, mut gone_otherwise, mut pending) = (0u64, 0u64, 0u64);
    // Each VM's start, from its create's sending to its start: the span its guest's uptime began in.
    let mut starts: Vec<Duration> = Vec::new();
    // Each window's readings come at least a window after the last's, however long an admission's create took: a
    // window is never a sliver caught up after a slow one.
    let mut next = Instant::now() + window.unwrap_or_default();
    while begun.elapsed() < run {
        hex_wait::rest_until(next).unwrap();
        let capacity = client
            .capacity(class)
            .unwrap()
            .expect("the class's capacity");
        let held = holdings(&capacity);
        // The learner's window as the class's capacity answer tells it, as the caps it holds move (C1).
        if let Some(told) = capacity.learned.as_ref().and_then(|l| l.window.as_ref()) {
            window = Some(Duration::from_nanos(told.window_ns));
        }
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
                let is_riser = admitted == u64::from(riser);
                let (trace, steps) = if is_riser {
                    longest
                } else {
                    &traces[usize::try_from(admitted).unwrap() % traces.len()]
                };
                let mut steps = steps.clone();
                if is_riser {
                    // The riser: its trace until its charge has warmed, then every vCPU and the most any trace holds.
                    let mut kept = Vec::new();
                    let mut spent = 0u64;
                    let after =
                        warm_up(window.expect("the learner's window, told before the riser"));
                    rises_after = Some(after);
                    let rise = u64::try_from(after.as_millis()).unwrap();
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
                let from = Instant::now();
                let info = client
                    .create(kind.spec(
                        id.clone(),
                        &player(trace, &steps, vcpus),
                        next_receipt,
                        workload,
                        image.as_deref(),
                    ))
                    .unwrap();
                let started = if info.state == BoxState::Created {
                    match client.start(&id) {
                        Ok(()) => Some(Instant::now()),
                        // A boot past its W* answers Pending (decision 9): it goes on, and a window to come finds it
                        // running.
                        Err(Refusal::Lifecycle {
                            cause: LifecycleCause::Pending,
                            ..
                        }) => {
                            pending += 1;
                            None
                        }
                        Err(why) => panic!("start {id}: {why}"),
                    }
                } else {
                    Some(Instant::now())
                };
                if let Some(started) = started {
                    starts.push(started.duration_since(from));
                }
                vms.push(Vm {
                    n,
                    id,
                    trace: trace.clone(),
                    asked: Asked::of(&steps, vcpus),
                    steps,
                    receipt: next_receipt,
                    from,
                    started,
                    last: None,
                    readings: 0,
                });
                admitted += 1;
                refused_at = None;
                if window.is_none() {
                    // No window told yet: the learner gives one once it holds this VM's cap, which the class's capacity
                    // answer then tells; the riser waits for it.
                    // A look: the class tells no event as its window moves, its capacity asked. Its party, the node,
                    // ends where an ask is refused, the node gone (the review's C-5).
                    let refused = std::cell::Cell::new(false);
                    hex_testkit::waits::follow::until(
                        &TOLD_WINDOW,
                        "the learner's window, told on the class's capacity answer",
                        || {
                            if refused.get() {
                                hex_testkit::waits::follow::Party::Ended
                            } else {
                                hex_testkit::waits::follow::Party::Live
                            }
                        },
                        || {
                            window = match client.capacity(class) {
                                Ok(capacity) => capacity
                                    .and_then(|c| c.learned)
                                    .and_then(|l| l.window)
                                    .map(|told| Duration::from_nanos(told.window_ns)),
                                Err(_) => {
                                    refused.set(true);
                                    None
                                }
                            };
                            window.is_some()
                        },
                    )
                    .unwrap();
                }
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
            && let Some(after) = rises_after
            && vms.iter().any(|vm| {
                vm.n == riser && vm.started.is_some_and(|started| started.elapsed() >= after)
            })
        {
            rise_window = Some(at);
        }
        let mut readings = Vec::new();
        let mut ending = Vec::new();
        let listed: BTreeMap<String, hex_api::BoxInfo> = client
            .list()
            .unwrap()
            .into_iter()
            .map(|info| (info.id.as_str().to_owned(), info))
            .collect();
        let seen = Instant::now();
        for vm in &mut vms {
            let info = listed.get(vm.id.as_str());
            let running = info.is_some_and(|info| info.state == BoxState::Running);
            let over = info.is_none_or(|info| {
                info.exit.is_some() || info.ended.is_some() || info.last_resort.is_some()
            });
            let started = match vm.started {
                Some(started) => started,
                // Its start answered Pending: it boots on until a window finds it running, or its end.
                None if running => {
                    vm.started = Some(seen);
                    starts.push(seen.duration_since(vm.from));
                    continue;
                }
                None if over => {
                    ending.push((vm.id.clone(), false));
                    continue;
                }
                None => continue,
            };
            // A VM whose trace has played ends with it: the driver deletes it, freeing its room. The riser's trace
            // lasts the run.
            if started.elapsed() >= length(&vm.steps) {
                ending.push((vm.id.clone(), true));
                continue;
            }
            // A VM that ended: its player's own end, having played (its exit 0, no last resort), else otherwise (the
            // last resort's end, its player's failure, or its run's).
            if !running {
                let own = info.is_some_and(|info| {
                    info.exit == Some(Exit::Code(0))
                        && info.last_resort.is_none()
                        && info.ended.is_none()
                });
                ending.push((vm.id.clone(), own));
                continue;
            }
            let sent = Instant::now();
            let Ok(usage) = client.usage(&vm.id) else {
                ending.push((vm.id.clone(), false));
                continue;
            };
            let this = Read {
                cpu_usec: usage.cpu_usec,
                sent,
                answered: Instant::now(),
            };
            let Some(last) = vm.last.replace(this) else {
                continue;
            };
            let (nominal, certain) = spans(&last, &this);
            let given = milli(
                this.cpu_usec.saturating_sub(last.cpu_usec),
                this.answered.duration_since(last.answered),
            );
            let asked = vm.asked_for_certain(started, &last, &this);
            let booked = scaled(
                held.get(vm.id.as_str())
                    .copied()
                    .unwrap_or(vm.receipt.booking())
                    .cpu_milli,
                certain,
                nominal,
            );
            vm.readings += 1;
            let vm_n = vm.n;
            readings.push(Reading {
                vm: vm_n,
                asked,
                given,
                booked,
            });
            writeln!(
                tsv,
                "{at}\t{vm_n}\t{}\t{}\t{}\t{asked}\t{given}\t{booked}",
                vm.trace,
                nominal / 1000,
                certain.max(0) / 1000
            )
            .unwrap();
        }
        let sent = Instant::now();
        let usage = client.usage(&boxed).unwrap();
        let this = Read {
            cpu_usec: usage.cpu_usec,
            sent,
            answered: Instant::now(),
        };
        if let Some(last) = box_last.replace(this) {
            let (nominal, certain) = spans(&last, &this);
            let given = milli(
                this.cpu_usec.saturating_sub(last.cpu_usec),
                this.answered.duration_since(last.answered),
            );
            // Its spinners ask its CPU whole from before the run's start.
            let asked = scaled(box_cpu, certain, nominal);
            let booked = scaled(box_booked.cpu_milli, certain, nominal);
            readings.push(Reading {
                vm: BOX,
                asked,
                given,
                booked,
            });
            writeln!(
                tsv,
                "{at}\tbox\t-\t{}\t{}\t{asked}\t{given}\t{booked}",
                nominal / 1000,
                certain.max(0) / 1000
            )
            .unwrap();
        }
        for (id, trace_played) in ending {
            if trace_played {
                played += 1;
            } else {
                gone_otherwise += 1;
                // Each VM that ended before its trace had played, by name, in its inspect's words.
                let info = client.inspect(&id).ok();
                let vm = vms.iter().find(|vm| vm.id == id);
                println!(
                    "HEX-CHARGE-FLEET ended vm={:?} id={id} trace={:?} riser={} after_ms={:?} exit={:?} \
                     last_resort={:?} ended={:?}",
                    vm.map(|vm| vm.n),
                    vm.map(|vm| vm.trace.as_str()),
                    vm.is_some_and(|vm| vm.n == riser),
                    vm.and_then(|vm| vm.started)
                        .map(|started| started.elapsed().as_millis()),
                    info.as_ref().and_then(|info| info.exit),
                    info.as_ref().and_then(|info| info.last_resort.as_ref()),
                    info.as_ref().and_then(|info| info.ended.as_ref()),
                );
            }
            // The riser is kept to the end, for its inspect.
            if id.as_str() != format!("charge-vm-{riser}")
                && let Err(why) = client.delete(&id)
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
        next = Instant::now() + window.unwrap_or_default();
    }
    let ran = begun.elapsed();
    let (learner, measured) = learner_cpu(node.child.id());
    let riser_id = BoxId::new(&format!("charge-vm-{riser}")).unwrap();
    let riser_info = client.inspect(&riser_id).ok();
    for vm in &vms {
        let _ = client.delete(&vm.id);
    }
    let _ = client.delete(&riser_id);
    let _ = client.delete(&boxed);
    fs::write(out.join("windows.tsv"), tsv).unwrap();

    let cast = Cast {
        riser: rise_window.map(|at| (riser, at)),
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
    starts.sort_unstable();
    let median = starts
        .get(starts.len().div_ceil(2).saturating_sub(1))
        .map(Duration::as_millis);
    let os = std::env::consts::OS;
    println!(
        "HEX-CHARGE-FLEET os={os} class={kind:?} window_ns={} windows={} ran_ms={} vms_peak={peak} \
         envelope_density={envelope} admitted={admitted} refused_for_room={refused} ended_with_their_traces={played} \
         ended_otherwise={gone_otherwise}",
        window.unwrap_or_default().as_nanos(),
        report.windows,
        ran.as_millis()
    );
    println!(
        "HEX-CHARGE-FLEET inputs workload_cpu_milli={} workload_mem_bytes={} allocation={:?} vcpus={vcpus} \
         box_asks_milli={box_cpu} box_booked={:?} traces={} image={image:?}",
        workload.cpu_milli,
        workload.mem_bytes,
        allocation.resources,
        box_booked,
        traces.len()
    );
    println!(
        "HEX-CHARGE-FLEET starts n={} pending={pending} uncertainty_ms_least={:?} median={median:?} most={:?}",
        starts.len(),
        starts.first().map(Duration::as_millis),
        starts.last().map(Duration::as_millis)
    );
    println!(
        "HEX-CHARGE-FLEET fairness contended={} ratio_n={} ratio_least_per_mille={} ratio_p1={} ratio_p50={} \
         misses={} jain_least_ppm={} jain_mean_ppm={} box_short={} floor_n={} floor_least_per_mille={} floor_p1={} \
         floor_p50={}",
        report.contended,
        report.ratios.n,
        report.ratios.least,
        report.ratios.p1,
        report.ratios.p50,
        report.misses,
        report.jain_least_ppm,
        report.jain_mean_ppm,
        report.box_short,
        report.floor.n,
        report.floor.least,
        report.floor.p1,
        report.floor.p50
    );
    let ended = riser_info
        .as_ref()
        .and_then(|info| info.last_resort.as_ref());
    println!(
        "HEX-CHARGE-FLEET riser rise_window={rise_window:?} windows={} over_booking={} given_of_asked_ppm={} \
         neighbours_short={} refused_while_rising={refused_rising} exit={:?} last_resort={ended:?}",
        report.riser_windows,
        report.riser_over,
        report.riser_given_ppm,
        report.neighbours_short,
        riser_info.as_ref().and_then(|info| info.exit)
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

/// Joint plan step 14 (AC-T7.4.1): the process box beside the fleet runs its spinners under the bounds it books, so
/// the run's CPU contention is real and its box's readings are its spinners'. Do run, in a macOS process box bounded as
/// the driver bounds it there (its booking, its memory alone), the driver's spinner for the box's CPU; expect the shell
/// and every spinner of it running, never its program ended first or the box refused (a bound that can't hold the
/// shell ends it at once, and every contended window would then read the box short).
#[cfg(target_os = "macos")]
#[test]
fn w14_the_box_beside_the_fleet_runs_its_spinners_under_its_bounds() {
    use hex_api::Runtime as _;
    use hex_core::patience::Patience;

    static RUNNING: Patience = Patience::measuring("test.hex_cli.charge_fleet.box_spinners");
    // A node's own macOS runtime, as `hex node` makes it.
    let model = hex_runtime::macos::machine_model().unwrap();
    let rates = hex_core::watch::seed(&model);
    let rt = hex_runtime::macos::MacRuntime::new(
        rates.bytes,
        rates.processes,
        hex_core::watch::lateness_seed(&model),
    )
    .unwrap();
    let caps = ci_box();
    let id = BoxId::new("w14-charge-box").unwrap();
    let _ = rt.delete(&id);
    rt.create(box_spec(id.clone(), box_booking(caps))).unwrap();
    let mut program = rt
        .exec(&id, spinner(caps.cpu_milli), Stdio::null())
        .unwrap();
    // The shell and one member a CPU of its cap.
    let members = caps.cpu_milli.div_ceil(1000) + 1;
    let mut ended = None;
    // A look: the box tells no event as its members start, its usage read. Its party, the program, ends the wait in
    // the fact itself.
    hex_testkit::waits::follow::until(
        &RUNNING,
        "the box's spinners running, or its program's end",
        || hex_testkit::waits::follow::Party::Live,
        || {
            if let Ok(Some(exit)) = program.try_wait() {
                ended = Some(exit);
                return true;
            }
            rt.usage(&id).is_ok_and(|usage| usage.pids >= members)
        },
    )
    .unwrap();
    let usage = rt.usage(&id).unwrap();
    rt.delete(&id).unwrap();
    assert_eq!(
        ended,
        None,
        "the box's spinner ended before its {members} members ran, under {:?}: {usage:?}",
        box_spec(id, box_booking(caps)).bounds
    );
}

/// Joint plan step 14 (AC-T7.4.1; the person's rule, 2026-11-13, and Shards' R03 for test waits): the driver times
/// none of its waits and reads no record. Each client call waits for its answer while the node's connection stands, past
/// its W* telling its run-out and following it (decision 9), and the node's start for its ready line while the node
/// runs.
/// Do read the driver's own source; expect no caller deadline, no timed receive and no record of boots read.
#[test]
fn w14_the_driver_times_no_wait_and_reads_no_record() {
    let source = include_str!("charge_fleet.rs");
    for timer in [
        concat!("_be", "fore("),
        concat!("recv_", "timeout"),
        concat!("guest::", "recorded"),
    ] {
        assert!(
            !source.contains(timer),
            "the driver still times a wait or reads a record: {timer}"
        );
    }
}

/// Joint plan step 14 (AC-T7.4.1): the process box is a CI job's, each of its caps derived from kiln's CI. Do reckon
/// the box from the CI trace; expect the nearest-rank medians of its 3,946 build legs: 363 milli-CPU and a peak of
/// 2,228,883,456 bytes, and no disk.
#[test]
fn w14_the_box_beside_the_fleet_is_a_median_ci_build_leg() {
    assert_eq!(
        ci_box(),
        CapacityResources {
            cpu_milli: 363,
            mem_bytes: 2_228_883_456,
            disk_bytes: 0,
        }
    );
}

/// A millisecond of a trace's time, in its ns.
const MS: i128 = 1_000_000;

/// Steps of `(millis, threads)`, holding no memory.
fn steps_of(pairs: &[(u64, u64)]) -> Vec<Step> {
    pairs
        .iter()
        .map(|&(millis, threads)| Step {
            millis,
            threads,
            mib: 0,
        })
        .collect()
}

/// Joint plan step 14 (AC-T7.4.1): a reading's ask is what its VM's trace asked for certain, the trace's zero placed
/// anywhere its VM's start allows (module docs). Do reckon a 100 ms reading of a trace that falls from 2 busy threads to
/// none at 1 s, the reading's start in the trace's time anywhere from 950 ms to 1 s; expect nothing asked for certain:
/// from 1 s, the trace may have fallen already.
#[test]
fn w14_a_reading_whose_start_spans_a_falling_edge_asks_nothing_for_certain() {
    let asked = Asked::of(&steps_of(&[(1_000, 2), (1_000, 0)]), 2);
    assert_eq!(asked.least(950 * MS, 1_000 * MS, 100 * MS), 0);
}

/// Joint plan step 14 (AC-T7.4.1): a step's end may come up to two ticks of `/proc/uptime` before its time, its deadline
/// and the trace's zero each read rounded down to a tick (module docs). Do reckon a 100 ms reading from 900 ms of a
/// trace falling from 2 busy threads to none at 1 s, its start certain; expect its 2 threads for the 80 ms before 980 ms.
#[test]
fn w14_a_falling_edge_is_certain_only_two_ticks_before_its_time() {
    let asked = Asked::of(&steps_of(&[(1_000, 2), (1_000, 0)]), 2);
    assert_eq!(
        asked.least(900 * MS, 900 * MS, 100 * MS),
        u128::try_from(2 * 80 * MS).unwrap()
    );
}

/// Joint plan step 14 (AC-T7.4.1): a step's start may come up to a tick of `/proc/uptime` past its time, the player's
/// sleep reckoned from a clock read rounded down (module docs). Do reckon a 100 ms reading from 1 s of a trace rising
/// from none to 2 busy threads at 1 s, its start certain; expect its 2 threads for the 90 ms from 1,010 ms.
#[test]
fn w14_a_rising_edge_is_certain_only_a_tick_after_its_time() {
    let asked = Asked::of(&steps_of(&[(1_000, 0), (1_000, 2)]), 2);
    assert_eq!(
        asked.least(1_000 * MS, 1_000 * MS, 100 * MS),
        u128::try_from(2 * 90 * MS).unwrap()
    );
}
