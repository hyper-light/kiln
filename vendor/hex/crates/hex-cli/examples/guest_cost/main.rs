//! AC-T7.4.1 producer of a VM class's `RuntimeCost` and its base evidence, measured on the host it runs on:
//! what a node reads as `HEX_GUEST_COST_FILE`, `HEX_GUEST_COST_SHA256` and `HEX_GUEST_COST_EVIDENCE`.
//!
//!   Linux:  cargo build --release --locked -p hex-cli --example guest_cost, then, in a cgroup delegated to
//!           it as the node's subtree is: systemd-run --user --scope -p Delegate=yes
//!             target/release/examples/guest_cost --class shards --out <dir>
//!   macOS:  cargo run --release --locked -p hex-cli --example guest_cost -- --class shards --out <dir>
//!             --cpu-response <row file> --cpu-response-sha256 <hex>
//!
//! with the class's guest configured as its node configures it (Shards: `SHARDS_BIN`, `HEX_GUEST_KERNEL`,
//! `HEX_GUEST_INITRD`, `HEX_GUEST_CPUS`, `HEX_GUEST_MEMORY_MIB`). On a Mac the host class's CPU-response
//! row comes first (`macos_cpu_response --out <dir>` writes it as a file of its own): its cycle is the
//! window the Mac's CPU control enforces, and its section heads the evidence, so the one pair carries both.
//!
//! A class is a driver on one seam ([`seam::Driver`]: launch, ready, idle, report, tear down, read through
//! its probes); the census, the sampling, the evidence and the output are shared ([`run`]). No input is
//! chosen; each is derived, and the evidence says how:
//! - floor = ceiling = the configured guest envelope (what `GuestRuntime::with_runtime_cost` requires);
//! - quantum = each dimension's unit, read here: the API's milli-CPU, the host's page size, and the block
//!   size of the filesystem the VMM's own files go on;
//! - window = the period of the CPU control the node enforces a VM's rate with: Linux's `cpu.max` period
//!   (`hex_core::cgroup::CPU_PERIOD_US`); the Mac's managed control's cycle for its CPU-response row at the
//!   VM's rate, the envelope's and the overhead's, taken to the fixed point a census settles at;
//! - overhead = the VMM's own cost outside the envelope, each guest read at every window boundary from its
//!   launch through its readiness and an idle hold: its own CPU in one window of its steady state, from its
//!   readiness on (its guest's left out; its boot's burst recorded apart, not charged); its own memory (Linux:
//!   its cgroup's memory.peak less its guest's touched pages; macOS: its footprint less them, guest ready);
//!   and the bytes held in its own folders (one-time host caches left out). guest_overhead = the kernel's and
//!   relay's use inside it, the envelope less what the guest agent reports free. Each is the maximum over
//!   SEEDABLE guests (decision 9's census rule, bench/competitive/probes/waits_census.py), each booted, told
//!   its deadline as the node tells it, within the class's recorded patience, doubled at each run-out. Both are
//!   evidence alone (joint plan step 14; ROOT, 2026-11-13, by decision 9's rule): the cost carries neither, and a
//!   node learns a VM's overhead and a guest's own from their windows.
// A measurement example, not production code (CLAUDE.md): the test crates' opt-outs.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    clippy::arithmetic_side_effects,
    clippy::disallowed_macros
)]

// The shared half reads its host's units through rustix, and the drivers so far are macOS's and Linux's.
#[cfg(any(target_os = "macos", target_os = "linux"))]
mod run;
#[cfg(any(target_os = "macos", target_os = "linux"))]
mod seam;
#[cfg(any(target_os = "macos", target_os = "linux"))]
mod shards;

#[cfg(any(target_os = "macos", target_os = "linux"))]
use std::path::PathBuf;
use std::process::ExitCode;

#[cfg(target_os = "linux")]
const USAGE: &str = "usage: guest_cost --class shards --out <dir>";
#[cfg(target_os = "macos")]
const USAGE: &str = "usage: guest_cost --class shards --out <dir> --cpu-response <row file> --cpu-response-sha256 <hex>";

/// The run's arguments: which class's driver, where its output goes, and on a Mac its host class's
/// CPU-response row file and that file's SHA-256. Exactly these, each once.
#[cfg(any(target_os = "macos", target_os = "linux"))]
struct Args {
    class: String,
    out: PathBuf,
    #[cfg(target_os = "macos")]
    cpu_response: (PathBuf, [u8; 32]),
}

#[cfg(any(target_os = "macos", target_os = "linux"))]
impl Args {
    fn parse(words: &[String]) -> Result<Self, String> {
        let (mut class, mut out) = (None, None);
        #[cfg(target_os = "macos")]
        let (mut row, mut row_sha256) = (None, None);
        let mut words = words.iter();
        while let Some(name) = words.next() {
            let slot = match name.as_str() {
                "--class" => &mut class,
                "--out" => &mut out,
                #[cfg(target_os = "macos")]
                "--cpu-response" => &mut row,
                #[cfg(target_os = "macos")]
                "--cpu-response-sha256" => &mut row_sha256,
                other => return Err(format!("unknown argument {other}\n{USAGE}")),
            };
            let value = words
                .next()
                .ok_or_else(|| format!("{name} needs a value\n{USAGE}"))?;
            if slot.replace(value.as_str()).is_some() {
                return Err(format!("{name} is given twice\n{USAGE}"));
            }
        }
        let missing = |name: &str| format!("{name} is required\n{USAGE}");
        Ok(Self {
            class: class.ok_or_else(|| missing("--class"))?.to_owned(),
            out: PathBuf::from(out.ok_or_else(|| missing("--out"))?),
            #[cfg(target_os = "macos")]
            cpu_response: (
                PathBuf::from(row.ok_or_else(|| missing("--cpu-response"))?),
                sha256(row_sha256.ok_or_else(|| missing("--cpu-response-sha256"))?)?,
            ),
        })
    }
}

/// 64 hex digits as the 32 bytes they spell.
#[cfg(target_os = "macos")]
fn sha256(digits: &str) -> Result<[u8; 32], String> {
    let wrong = || "--cpu-response-sha256 must be 64 hex digits".to_owned();
    if digits.len() != 64 {
        return Err(wrong());
    }
    let mut bytes = [0u8; 32];
    for (byte, at) in bytes.iter_mut().zip((0..64).step_by(2)) {
        *byte = digits
            .get(at..at + 2)
            .and_then(|pair| u8::from_str_radix(pair, 16).ok())
            .ok_or_else(wrong)?;
    }
    Ok(bytes)
}

#[cfg(not(any(target_os = "macos", target_os = "linux")))]
fn main() -> ExitCode {
    eprintln!("guest_cost has no class driver on {}", std::env::consts::OS);
    ExitCode::FAILURE
}

#[cfg(any(target_os = "macos", target_os = "linux"))]
fn main() -> ExitCode {
    let words: Result<Vec<String>, String> = std::env::args_os()
        .skip(1)
        .map(|word| {
            word.into_string()
                .map_err(|_| "arguments must be UTF-8".to_owned())
        })
        .collect();
    match words.and_then(|words| produce(&Args::parse(&words)?)) {
        Ok(made) => {
            println!("{}", made.summary);
            println!("samples {}", made.samples_file.display());
            println!("HEX_GUEST_COST_FILE={}", made.cost_file.display());
            println!("HEX_GUEST_COST_SHA256={}", run::hex(&made.cost_sha256));
            println!("HEX_GUEST_COST_EVIDENCE={}", made.evidence_file.display());
            ExitCode::SUCCESS
        }
        Err(why) => {
            eprintln!("guest_cost: {why}");
            ExitCode::FAILURE
        }
    }
}

#[cfg(any(target_os = "macos", target_os = "linux"))]
fn produce(args: &Args) -> Result<run::Made, String> {
    let plan = run::Plan::derived()?;
    match args.class.as_str() {
        "shards" => {
            let (windows, prefix) = windows(args)?;
            let mut driver = shards::Shards::from_env(&args.out)?;
            run::produce(&mut driver, &plan, &args.out, &windows, &prefix)
        }
        other => Err(format!(
            "no driver for class {other} on {}\n{USAGE}",
            std::env::consts::OS
        )),
    }
}

/// Linux's window: the `cpu.max` period the node writes for a VM's cgroup, whatever the VM's rate. No section
/// heads the evidence.
#[cfg(target_os = "linux")]
fn windows(_: &Args) -> Result<(run::Windows, Vec<u8>), String> {
    Ok((
        run::Windows::Fixed(run::Window {
            length: std::time::Duration::from_micros(hex_core::cgroup::CPU_PERIOD_US),
            source: "hex_core::cgroup::CPU_PERIOD_US, the cpu.max period the node writes for a VM's cgroup"
                .to_owned(),
        }),
        Vec::new(),
    ))
}

/// The Mac's windows: its managed CPU control's cycle for this host class's CPU-response row, at a VM's
/// rate; the row file, checked against its SHA-256 and holding rows alone, heads the evidence as it is.
#[cfg(target_os = "macos")]
fn windows(args: &Args) -> Result<(run::Windows, Vec<u8>), String> {
    use sha2::{Digest, Sha256};
    use std::io::Read as _;
    let (path, expected) = &args.cpu_response;
    // As much as a node reads of its cost inputs, and one byte more to tell a longer file.
    let most = hex_core::bundle::MAX_CONFIG_BYTES;
    let mut section = Vec::new();
    std::fs::File::open(path)
        .and_then(|file| {
            file.take(u64::try_from(most + 1).unwrap_or(u64::MAX))
                .read_to_end(&mut section)
        })
        .map_err(|e| format!("reading {}: {e}", path.display()))?;
    if section.len() > most {
        return Err(format!("{} is longer than a node reads", path.display()));
    }
    let actual: [u8; 32] = Sha256::digest(&section).into();
    if &actual != expected {
        return Err(format!(
            "{} has SHA-256 {}, not the one given",
            path.display(),
            run::hex(&actual)
        ));
    }
    let (rows, rest) =
        hex_runtime::guest::cpu_response_split(&section).map_err(|e| e.to_string())?;
    if rows.is_empty() || !rest.is_empty() {
        return Err(format!(
            "{} is no CPU-response row file: macos_cpu_response --out writes one",
            path.display()
        ));
    }
    let host = hex_runtime::macos::machine_model().map_err(|e| e.to_string())?;
    let row = rows.iter().find(|row| row.host == host).ok_or_else(|| {
        format!(
            "{} has no row for host class {host}: run macos_cpu_response --out on it first",
            path.display()
        )
    })?;
    let response = row.response();
    let ceiling = hex_runtime::macos::cpu_ceiling().map_err(|e| e.to_string())?;
    let named = format!(
        "R = {} ns, the max of host class {host}'s CPU-response row (row file {} sha256={}, its samples sha256={})",
        response.as_nanos(),
        path.display(),
        run::hex(expected),
        run::hex(&row.raw)
    );
    let cycle = move |rate: u64| -> Result<run::Window, String> {
        let (run_for, held_for) =
            hex_runtime::macos::cpu_control_cycle(rate, response).map_err(|e| e.to_string())?;
        Ok(run::Window {
            length: run_for.checked_add(held_for).ok_or("the cycle overflows")?,
            source: format!(
                "the managed CPU control's cycle at the rate r={rate} milli-CPU: run burst/(M-r) {} ns, then held 2*burst/r {} ns, burst = M*R, M = hw.logicalcpu_max*1000 = {ceiling}, {named}",
                run_for.as_nanos(),
                held_for.as_nanos(),
            ),
        })
    };
    Ok((run::Windows::AtRate(Box::new(cycle)), section))
}

/// The smoke's arguments: its folder, and on a Mac its row file from the environment.
#[cfg(all(test, any(target_os = "macos", target_os = "linux")))]
fn smoke_args(out: &std::path::Path) -> Args {
    Args {
        class: "shards".to_owned(),
        out: out.to_owned(),
        #[cfg(target_os = "macos")]
        cpu_response: (
            PathBuf::from(std::env::var_os("GUEST_COST_SMOKE_ROW").expect("GUEST_COST_SMOKE_ROW")),
            sha256(
                &std::env::var("GUEST_COST_SMOKE_ROW_SHA256").expect("GUEST_COST_SMOKE_ROW_SHA256"),
            )
            .unwrap(),
        ),
    }
}

#[cfg(all(test, any(target_os = "macos", target_os = "linux")))]
mod tests {
    use super::*;

    fn words(list: &[&str]) -> Vec<String> {
        list.iter().map(|w| (*w).to_owned()).collect()
    }

    /// AC-T7.4.1: the producer takes exactly its class and output folder (and on a Mac its row file and
    /// that file's SHA-256): expect a run's parameters, a repeated or unknown argument, refused.
    #[test]
    fn the_producer_takes_its_class_and_folder_alone() {
        #[cfg(target_os = "linux")]
        let needed: Vec<&str> = Vec::new();
        #[cfg(target_os = "macos")]
        let needed = vec![
            "--cpu-response",
            "/x/row",
            "--cpu-response-sha256",
            "ab01ab01ab01ab01ab01ab01ab01ab01ab01ab01ab01ab01ab01ab01ab01ab01",
        ];
        let mut given = vec!["--class", "shards", "--out", "/tmp/x"];
        given.extend(needed.iter().copied());
        let args = Args::parse(&words(&given)).unwrap();
        assert_eq!(
            (args.class.as_str(), args.out),
            ("shards", PathBuf::from("/tmp/x"))
        );
        assert!(Args::parse(&words(&["--class", "shards"])).is_err());
        let mut twice = vec!["--class", "a", "--class", "b", "--out", "o"];
        twice.extend(needed.iter().copied());
        assert!(Args::parse(&words(&twice)).is_err());
        let mut unknown = vec!["--samples", "3", "--class", "shards", "--out", "o"];
        unknown.extend(needed.iter().copied());
        assert!(Args::parse(&words(&unknown)).is_err());
        #[cfg(target_os = "macos")]
        {
            assert!(Args::parse(&words(&["--class", "shards", "--out", "o"])).is_err());
            let mut short = vec!["--class", "shards", "--out", "o"];
            short.extend(["--cpu-response", "/x/row", "--cpu-response-sha256", "ab"]);
            assert!(Args::parse(&words(&short)).is_err());
        }
        #[cfg(target_os = "linux")]
        assert!(
            Args::parse(&words(&[
                "--class",
                "shards",
                "--out",
                "o",
                "--cpu-response",
                "/x/row"
            ]))
            .is_err()
        );
    }

    /// AC-T7.4.1: the windows on a Mac come from a row file whose SHA-256 is given: expect the control's cycle
    /// for this host's row at the rate asked (its run then its hold), the file at the evidence's head as it
    /// is, and a file that doesn't match its SHA-256, holds more than rows, or has no row for this host
    /// refused.
    #[cfg(target_os = "macos")]
    #[test]
    fn the_macs_window_is_its_controls_cycle_for_its_row() {
        use sha2::{Digest, Sha256};
        use std::time::Duration;
        let dir = tempfile::tempdir().unwrap();
        let host = hex_runtime::macos::machine_model().unwrap();
        let row = |host: &str| hex_runtime::guest::CpuResponse {
            host: host.to_owned(),
            samples: 3,
            p50: Duration::from_micros(50),
            p90: Duration::from_micros(70),
            p99: Duration::from_micros(110),
            max: Duration::from_micros(117),
            raw: [0xab; 32],
        };
        let write = |name: &str, bytes: &[u8]| -> (PathBuf, [u8; 32]) {
            let path = dir.path().join(name);
            std::fs::write(&path, bytes).unwrap();
            (path, Sha256::digest(bytes).into())
        };
        let args = |cpu_response| Args {
            class: "shards".to_owned(),
            out: dir.path().to_owned(),
            cpu_response,
        };
        let section = hex_runtime::guest::cpu_response_section(&[row(&host)]).unwrap();
        let (cycles, prefix) = windows(&args(write("row", &section))).unwrap();
        for rate in [1000, 2536] {
            let (run_for, held_for) =
                hex_runtime::macos::cpu_control_cycle(rate, Duration::from_micros(117)).unwrap();
            let cycle = cycles.at(rate).unwrap();
            assert_eq!(cycle.length, run_for + held_for);
            assert!(cycle.source.contains(&host), "{}", cycle.source);
        }
        assert!(cycles.at(1000).unwrap().length > cycles.at(2536).unwrap().length);
        assert_eq!(prefix, section);

        let (path, _) = write("row", &section);
        assert!(windows(&args((path, [0; 32]))).is_err());
        let mut more = section.clone();
        more.extend_from_slice(b"hex-guest-cost 2\n");
        assert!(windows(&args(write("more", &more))).is_err());
        let other = hex_runtime::guest::cpu_response_section(&[row("Other0,1")]).unwrap();
        assert!(windows(&args(write("other", &other))).is_err());
    }
}
