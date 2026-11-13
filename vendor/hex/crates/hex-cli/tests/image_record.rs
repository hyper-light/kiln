//! The Shards image class's wait record on one host class (joint plan step 12, AC-T7.4.1; decision 9;
//! docs/design/shards-image-class.md §6): the slowest answer of each of its three waits, the rows of
//! bench/native/guest/records.tsv that seed them and turn its creates on. `image_boot`: `Images::boot` of a harness
//! image to its return, its members started. `image_stats`: `stats(StatsScope::Run)` to its answer. `image_stop`:
//! `stop` to its return, the run's end told and its VM gone. Each run boots the image through
//! `shards_api::image::ImageShards` as the class does: its members alone, under `hex_runtime::grants::program`'s
//! default deny, its run host given the home whose store holds the image, a cache, and the kernel and init it boots.
//!
//! Each series starts as a node's first boot after an upgrade does: a cache of its own, so its first run host writes
//! Shards' helpers out; its other runs find them there, as a node's later boots do. A series' cache goes as it ends.
//! Each run's line is written as the run ends, beside the host's 1-minute load average read before its boot. A
//! refused boot, stats read or stop fails the record, naming the run, and keeps the lines before it. The record's
//! deadline bounds every call: a boot's and a stats read's patience is what is left of it, and a stop, which takes
//! none, is waited for on a thread of its own for what is left.
//!
//! Writes into `IMAGE_RECORD_OUT`, made here:
//! - `raw.tsv`: each run's series, index, load, boot, stats reads and stop, in microseconds;
//! - `rows.tsv`: the three records.tsv rows for this host class, each its measure's slowest, rounded up to the
//!   microsecond;
//! - `RECORD.md`: the record's facts, for its page (bench/native/guest/image/RECORD.md): the host, its kernel and
//!   load, Hex's revision, the `shards` binary, the guest's kernel and init and the image, and n with each measure's
//!   min, p50, p99 and max, and raw.tsv's SHA-256.
//!
//! Explicit inputs only, each by its own variable; a missing one fails by name:
//! - `SHARDS_BIN`: the run hosts' `shards`, built at Hex's vendored Shards revision (vendor/MANIFEST.toml), so it
//!   speaks the image channel Hex's `shards_api` does; `SHARDS_KERNEL` and `SHARDS_INIT`: the kernel its runs boot,
//!   Shards' pinned one (its `crates/shards/src/kernel.rs`), so no run downloads one, and the shards-init that
//!   `shards` build carries (its build.rs's `OUT_DIR/shards-init`), which a run takes only beside a kernel given so
//!   ("--kernel and --init (or SHARDS_KERNEL and SHARDS_INIT) go together", Shards' run.rs);
//! - `IMAGE_RECORD_HOME`: the Shards home whose store holds the image; `IMAGE_RECORD_IMAGE`: the image, pinned
//!   (`NAME@sha256:...`), an Agentfile image that runs at least one harness: on a host with no harness build of its
//!   arch, the in-tree one (`crates/hex-cli/tests/image_harness`, which any arch builds), built into that home by
//!   `shards build image -t NAME --metadata-file FILE crates/hex-cli/tests/image_harness` with `SHARDS_HOME` set to
//!   it, and pinned by the `containerimage.digest` FILE says;
//! - `IMAGE_RECORD_VCPUS` and `IMAGE_RECORD_MEMORY_MIB`: each VM's size, the node's guest envelope, as an image VM's
//!   admission sizes it (`Allocation::vm_size`);
//! - `IMAGE_RECORD_SERIES` and `IMAGE_RECORD_RUNS`: the series, and the runs in each, n their product (with n = 299
//!   the slowest bounds a p99 at 95%: 0.99^299 < 0.05); `IMAGE_RECORD_STATS`: the stats reads in each run;
//! - `IMAGE_RECORD_WAIT_MS`: the record's deadline; `IMAGE_RECORD_OUT`: a new directory.
//!
//! Run: `cargo test --release --locked -p hex-cli --test image_record -- --ignored --nocapture` with those set.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    clippy::arithmetic_side_effects,
    clippy::disallowed_macros
)]

use std::ffi::OsString;
use std::fmt::Write as _;
use std::fs;
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{Arc, mpsc};
use std::time::{Duration, Instant};

use hex_api::ShardsImage;
use hex_runtime::guest::CLASS;
use sha2::{Digest, Sha256};
use shards_api::image::{
    ImageGuest, ImageShards, ImageSpec, Images, MemberKind, RunEnd, StatsScope,
};

/// Where the record's page goes, which its rows name.
const PAGE: &str = "bench/native/guest/image/RECORD.md";

/// The variable `name`'s value, which the record needs: failing by name where it is unset.
fn input(name: &str) -> String {
    std::env::var(name)
        .unwrap_or_else(|_| panic!("{name} is unset (crates/hex-cli/tests/image_record.rs)"))
}

fn number(name: &str) -> u64 {
    input(name)
        .parse()
        .unwrap_or_else(|e| panic!("{name}: {e}"))
}

/// What is left of the record's deadline.
fn left(until: Instant) -> Duration {
    until.saturating_duration_since(Instant::now())
}

/// The host's 1-minute load average, as its OS reports it, or `unread`.
#[cfg(target_os = "linux")]
fn load() -> String {
    fs::read_to_string("/proc/loadavg")
        .ok()
        .and_then(|line| line.split_whitespace().next().map(str::to_owned))
        .unwrap_or_else(|| "unread".to_owned())
}

#[cfg(target_os = "macos")]
fn load() -> String {
    hex_runtime::macos::sys::loadavg()
        .map_or_else(|| "unread".to_owned(), |load| format!("{load:.2}"))
}

/// Windows keeps no load average.
#[cfg(not(any(target_os = "linux", target_os = "macos")))]
fn load() -> String {
    "unread".to_owned()
}

/// A command's first line of output, or `unread`: the record's facts about its host and tree.
fn said(program: &str, args: &[&str]) -> String {
    Command::new(program)
        .args(args)
        .output()
        .ok()
        .filter(|out| out.status.success())
        .and_then(|out| {
            String::from_utf8(out.stdout)
                .ok()?
                .lines()
                .next()
                .map(str::to_owned)
        })
        .unwrap_or_else(|| "unread".to_owned())
}

/// A file's SHA-256, in hex.
fn sha256_of(path: &Path) -> String {
    let bytes = fs::read(path).unwrap_or_else(|e| panic!("reading {}: {e}", path.display()));
    Sha256::digest(&bytes)
        .iter()
        .fold(String::new(), |mut hex, byte| {
            write!(hex, "{byte:02x}").unwrap();
            hex
        })
}

/// A duration in whole microseconds, rounded up, so a row never understates what it records.
fn micros(d: Duration) -> u128 {
    d.as_nanos().div_ceil(1_000)
}

/// A duration in milliseconds to the microsecond, as the record's page says it.
fn ms(d: Duration) -> String {
    format!("{:.3}", d.as_secs_f64() * 1_000.0)
}

/// The image Hex's image class boots for `image` at `vcpus` and `memory_mib`: its members alone, its network as a VM
/// granted nothing is programmed, and its run host given `env`.
fn spec(
    image: &str,
    vcpus: u32,
    memory_mib: u64,
    shards: &Path,
    env: Vec<(OsString, OsString)>,
) -> ImageSpec {
    let network = hex_runtime::grants::program(&ShardsImage::new(image)).unwrap();
    ImageSpec {
        image: image.to_owned(),
        vcpus,
        memory_mib,
        command: None,
        members: Vec::new(),
        publish: network.publish,
        egress: network.egress,
        shares: Vec::new(),
        nesting: false,
        ready_by: None,
        credentials: Vec::new(),
        lessor: None,
        binary: Some(shards.to_path_buf()),
        env,
        control: None,
        gpu_devices: Vec::new(),
    }
}

/// One run's answers: its boot, each stats read and its stop, and its members as its boot named them.
struct Run {
    boot: Duration,
    stats: Vec<Duration>,
    stop: Duration,
    members: String,
}

/// Boots `spec`, reads its run's stats `reads` times and stops it, each within what is left of `until`.
fn once(images: &ImageShards, spec: &ImageSpec, reads: u64, until: Instant) -> Result<Run, String> {
    let began = Instant::now();
    let booted = images
        .boot(spec, left(until))
        .map_err(|e| format!("its boot was refused: {e}"))?;
    let boot = began.elapsed();
    let guest: Arc<dyn ImageGuest> = Arc::from(booted);
    let members = guest
        .members()
        .iter()
        .map(|m| {
            format!(
                "{:?} {}{}",
                m.kind,
                m.name,
                if m.runs { "" } else { " (files alone)" }
            )
        })
        .collect::<Vec<_>>()
        .join(", ");
    if !guest
        .members()
        .iter()
        .any(|m| m.kind == MemberKind::Harness && m.runs)
    {
        return Err(format!(
            "the image runs no harness: its members are [{members}]"
        ));
    }
    let mut stats = Vec::new();
    for _ in 0..reads {
        let asked = Instant::now();
        let read = guest
            .stats(StatsScope::Run, left(until))
            .map_err(|e| format!("a stats read was refused: {e}"))?;
        stats.push(asked.elapsed());
        // What the class's usage reads of them: each said, or its usage refuses it (hex_runtime::guest's image usage).
        if [
            read.cpu_ns,
            read.memory_bytes,
            read.pids,
            read.read_bytes,
            read.write_bytes,
        ]
        .iter()
        .any(Option::is_none)
        {
            return Err(format!(
                "its stats lack a value the class's usage reads: {read:?}"
            ));
        }
    }
    let (said, heard) = mpsc::channel();
    let stopping = Arc::clone(&guest);
    let asked = Instant::now();
    std::thread::spawn(move || {
        let stopped = stopping.stop();
        let _ = said.send((stopped, stopping.wait_for(Duration::ZERO)));
    });
    let (stopped, end) = heard
        .recv_timeout(left(until))
        .map_err(|e| format!("its stop did not return within the record's deadline: {e}"))?;
    let stop = asked.elapsed();
    stopped.map_err(|e| format!("its stop was refused: {e}"))?;
    match end {
        Ok(Some(RunEnd::Stopped)) => Ok(Run {
            boot,
            stats,
            stop,
            members,
        }),
        other => Err(format!("its run's end after its stop: {other:?}")),
    }
}

/// n, min, p50, p99 and max of `samples` (nearest rank), which holds at least one.
struct Summary {
    n: usize,
    min: Duration,
    p50: Duration,
    p99: Duration,
    max: Duration,
}

fn summary(mut samples: Vec<Duration>) -> Summary {
    samples.sort();
    let n = samples.len();
    let rank = |per_mille: usize| samples[(n * per_mille).div_ceil(1_000).max(1) - 1];
    Summary {
        n,
        min: samples[0],
        p50: rank(500),
        p99: rank(990),
        max: samples[n - 1],
    }
}

#[test]
#[ignore = "a measurement: run with --ignored --nocapture, release, on the host class it records"]
fn image_boot_stats_and_stop() {
    let shards = PathBuf::from(input("SHARDS_BIN"));
    let kernel = PathBuf::from(input("SHARDS_KERNEL"));
    assert!(kernel.is_file(), "SHARDS_KERNEL {kernel:?} is no file");
    let init = PathBuf::from(input("SHARDS_INIT"));
    assert!(init.is_file(), "SHARDS_INIT {init:?} is no file");
    let home = std::path::absolute(input("IMAGE_RECORD_HOME")).unwrap();
    assert!(home.is_dir(), "IMAGE_RECORD_HOME {home:?} is no directory");
    let image = input("IMAGE_RECORD_IMAGE");
    ShardsImage::new(&image).check().unwrap();
    let vcpus = u32::try_from(number("IMAGE_RECORD_VCPUS")).unwrap();
    let memory_mib = number("IMAGE_RECORD_MEMORY_MIB");
    let (series, runs, reads) = (
        number("IMAGE_RECORD_SERIES"),
        number("IMAGE_RECORD_RUNS"),
        number("IMAGE_RECORD_STATS"),
    );
    assert!(
        series > 0 && runs > 0 && reads > 0,
        "IMAGE_RECORD_SERIES, IMAGE_RECORD_RUNS and IMAGE_RECORD_STATS are each at least 1"
    );
    let wait = Duration::from_millis(number("IMAGE_RECORD_WAIT_MS"));
    let out = std::path::absolute(input("IMAGE_RECORD_OUT")).unwrap();
    assert!(!out.exists(), "a record's own directory: {out:?} exists");
    fs::create_dir_all(&out).unwrap();
    let begun = said("date", &["-u", "+%Y-%m-%dT%H:%M:%SZ"]);
    let until = Instant::now() + wait;
    let images = ImageShards::new();
    let mut raw = fs::File::create(out.join("raw.tsv")).unwrap();
    writeln!(raw, "# series\trun\tload1\tboot_us\tstats_us\tstop_us").unwrap();
    println!(
        "IMAGE-RECORD class={CLASS} image={image} n={} stats_reads={reads}",
        series * runs
    );
    let (mut boots, mut stats, mut stops, mut loads) =
        (Vec::new(), Vec::new(), Vec::new(), Vec::new());
    let (mut first_boots, mut members) = (Vec::new(), String::new());
    for s in 1..=series {
        let cache = out.join(format!("cache-{s}"));
        fs::create_dir(&cache).unwrap();
        let env = vec![
            (OsString::from("SHARDS_HOME"), home.clone().into_os_string()),
            (
                OsString::from("SHARDS_CACHE"),
                cache.clone().into_os_string(),
            ),
            (
                OsString::from("SHARDS_KERNEL"),
                kernel.clone().into_os_string(),
            ),
            (OsString::from("SHARDS_INIT"), init.clone().into_os_string()),
        ];
        let spec = spec(&image, vcpus, memory_mib, &shards, env);
        for r in 1..=runs {
            let load = load();
            let run = once(&images, &spec, reads, until)
                .unwrap_or_else(|why| panic!("series {s} run {r}: {why}"));
            let read_us = run
                .stats
                .iter()
                .map(|d| micros(*d).to_string())
                .collect::<Vec<_>>()
                .join(",");
            let line = format!(
                "{s}\t{r}\t{load}\t{}\t{read_us}\t{}",
                micros(run.boot),
                micros(run.stop)
            );
            writeln!(raw, "{line}").unwrap();
            println!("IMAGE-RECORD run {line}");
            if r == 1 {
                first_boots.push(run.boot);
            }
            if members.is_empty() {
                members = run.members;
            }
            boots.push(run.boot);
            stats.extend(run.stats);
            stops.push(run.stop);
            loads.extend(load.parse::<f64>().ok());
        }
        fs::remove_dir_all(&cache).unwrap();
    }
    drop(raw);
    let ended = said("date", &["-u", "+%Y-%m-%dT%H:%M:%SZ"]);
    let measures = [
        ("image_boot", summary(boots)),
        ("image_stats", summary(stats)),
        ("image_stop", summary(stops)),
    ];
    let mut rows = String::new();
    for (measure, s) in &measures {
        writeln!(rows, "{measure}\t{CLASS}\t{}\t{PAGE}", micros(s.max)).unwrap();
    }
    fs::write(out.join("rows.tsv"), &rows).unwrap();
    loads.sort_by(f64::total_cmp);
    let load_line = match (loads.first(), loads.get(loads.len() / 2), loads.last()) {
        (Some(min), Some(p50), Some(max)) => format!("min {min:.2} / p50 {p50:.2} / max {max:.2}"),
        _ => "unread".to_owned(),
    };
    let tree = env!("CARGO_MANIFEST_DIR");
    let revision = said("git", &["-C", tree, "rev-parse", "HEAD"]);
    let changed = Command::new("git")
        .args(["-C", tree, "status", "--porcelain"])
        .output()
        .ok()
        .filter(|out| out.status.success())
        .map_or("unread".to_owned(), |out| {
            String::from_utf8_lossy(&out.stdout)
                .lines()
                .count()
                .to_string()
        });
    let mut page = String::new();
    writeln!(
        page,
        "# Shards image VMs' boot, stats and stop on {CLASS} (joint plan step 12; decision 9)\n"
    )
    .unwrap();
    writeln!(
        page,
        "Taken by `crates/hex-cli/tests/image_record.rs`: each run boots the image through shards-api's \
         `ImageShards` as Hex's image class does (its members alone, nothing granted past the VM), reads its run's \
         stats {reads} times and stops it. Each measure's slowest is its records.tsv row, and a wait's bound is \
         twice its row (crates/hex-client/src/guest/shards.rs).\n"
    )
    .unwrap();
    writeln!(
        page,
        "- host: {}, {}, {} CPUs",
        said("hostname", &[]),
        said("uname", &["-srm"]),
        std::thread::available_parallelism().map_or(0, std::num::NonZero::get)
    )
    .unwrap();
    writeln!(
        page,
        "- when: {begun} to {ended}; load (1-minute, before each boot): {load_line}"
    )
    .unwrap();
    writeln!(
        page,
        "- Hex: {revision} ({changed} paths changed beside it)"
    )
    .unwrap();
    writeln!(
        page,
        "- shards: `{}`, sha256 `{}`",
        shards.display(),
        sha256_of(&shards)
    )
    .unwrap();
    writeln!(
        page,
        "- kernel: `{}`, sha256 `{}`; init: `{}`, sha256 `{}`",
        kernel.display(),
        sha256_of(&kernel),
        init.display(),
        sha256_of(&init)
    )
    .unwrap();
    writeln!(page, "- image: `{image}`; its members: {members}").unwrap();
    writeln!(
        page,
        "- each VM: {vcpus} vCPUs, {memory_mib} MiB; {series} series of {runs} runs, each series' first run with a \
         cache of its own (an upgrade's first boot)"
    )
    .unwrap();
    writeln!(
        page,
        "- raw: raw.tsv, sha256 `{}`\n",
        sha256_of(&out.join("raw.tsv"))
    )
    .unwrap();
    writeln!(
        page,
        "| measure | n | min ms | p50 ms | p99 ms | max ms (its row) |"
    )
    .unwrap();
    writeln!(page, "|---|---|---|---|---|---|").unwrap();
    for (measure, s) in &measures {
        writeln!(
            page,
            "| {measure} | {} | {} | {} | {} | **{}** |",
            s.n,
            ms(s.min),
            ms(s.p50),
            ms(s.p99),
            ms(s.max)
        )
        .unwrap();
    }
    let firsts = first_boots
        .iter()
        .map(|d| ms(*d))
        .collect::<Vec<_>>()
        .join(" ");
    writeln!(
        page,
        "\nEach series' first boot (an upgrade's), ms: {firsts}"
    )
    .unwrap();
    fs::write(out.join("RECORD.md"), &page).unwrap();
    println!("IMAGE-RECORD rows\n{rows}");
    println!("IMAGE-RECORD page\n{page}");
}
