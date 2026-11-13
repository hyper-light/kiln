//! AC-T7.4.1 producer of the macOS managed CPU control's measured response. On this Mac it measures what
//! the native watcher's stop costs: a one-shot `NOTE_CRITICAL` kqueue timer (the watcher's own,
//! `Kqueue::wait`) is armed, and when it fires a running member is stopped and read until the kernel
//! reports it stopped. One sample is the time from the timer's due instant to that observed stop: the
//! wake's lateness plus the stop's delivery. It writes the distribution as this host class's row, the
//! samples beside it, and either
//! - given `--out` alone, the row as a file of its own (the CPU-response section with that one row), whose
//!   SHA-256 it prints: `guest_cost` takes it, as its window and its evidence's head, so a Mac class's
//!   first pair is this row file and then `guest_cost`; or
//! - given a cost pair, the row at the head of its evidence (the producer's own section, replacing an older
//!   row of the same host) and the cost re-keyed to the new evidence's SHA-256.
//!
//! cargo run --release -p hex-cli --example macos_cpu_response -- --out <dir>
//! cargo run --release -p hex-cli --example macos_cpu_response -- --cost-file <cost.json>
//!   --cost-sha256 <hex> --evidence <evidence> --out <dir>
//!
//! No input of the measurement is chosen; each is derived, and the samples file's `#` lines say how:
//! - samples: the census rule's `SEEDABLE` (bench/competitive/probes/waits_census.py, decision 9), the
//!   fewest a row seeds from, read from the census itself;
//! - bound: the guest's recorded first dial bound (`hex_runtime::guest::recorded_dial_bound`), the
//!   node's managed helper's control patience; a stop not seen within it refuses the run;
//! - waits: the watcher arms its timer for whatever credit is left, so the arms run from the timer's own
//!   resolution up to the bound, one per binary order of magnitude of its microsecond count, then the
//!   bound, each taken the same number of times, round-robin. The row's max covers every arm.
// A measurement example, not production code (CLAUDE.md): the test crates' opt-outs.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    clippy::arithmetic_side_effects,
    clippy::disallowed_macros
)]

use std::process::ExitCode;

#[cfg(target_os = "macos")]
fn main() -> ExitCode {
    producer::run()
}

#[cfg(not(target_os = "macos"))]
fn main() -> ExitCode {
    eprintln!("macos_cpu_response measures a macOS host; run it on the Mac whose row it writes");
    ExitCode::FAILURE
}

#[cfg(target_os = "macos")]
mod producer {
    use std::path::PathBuf;
    use std::process::ExitCode;
    use std::time::Duration;

    const USAGE: &str = "usage: macos_cpu_response --out <dir>, or macos_cpu_response --cost-file <cost.json> --cost-sha256 <hex> --evidence <evidence> --out <dir>";

    /// The census whose rule names the fewest samples a row seeds from (decision 9).
    const CENSUS: &str = include_str!("../../../bench/competitive/probes/waits_census.py");

    /// The watcher timer's resolution: `Kqueue::wait` arms it in whole microseconds (`NOTE_USECONDS`).
    const RESOLUTION: Duration = Duration::from_micros(1);

    /// A run's samples, derived as the head of this file says, never an argument.
    struct Plan {
        seedable: usize,
        bound: Duration,
        arms: Vec<Duration>,
        per_arm: usize,
    }

    impl Plan {
        fn derived() -> Result<Self, String> {
            let seedable = CENSUS
                .lines()
                .find_map(|line| line.strip_prefix("SEEDABLE = "))
                .and_then(|n| n.trim().replace('_', "").parse::<usize>().ok())
                .ok_or("bench/competitive/probes/waits_census.py names no SEEDABLE")?;
            let bound = hex_runtime::guest::recorded_dial_bound().map_err(|e| e.to_string())?;
            Self::new(seedable, bound)
        }

        /// Arms from [`RESOLUTION`] doubling while below `bound`, then `bound`: at most one per bit of the
        /// bound's count of resolutions, so the loop ends.
        fn new(seedable: usize, bound: Duration) -> Result<Self, String> {
            if seedable == 0 || bound < RESOLUTION {
                return Err(format!(
                    "no samples to take for SEEDABLE {seedable} and bound {bound:?}"
                ));
            }
            let mut arms = Vec::new();
            let mut arm = RESOLUTION;
            while arm < bound {
                arms.push(arm);
                arm = arm.saturating_mul(2);
            }
            arms.push(bound);
            Ok(Self {
                seedable,
                bound,
                per_arm: seedable.div_ceil(arms.len()),
                arms,
            })
        }

        fn samples(&self) -> usize {
            self.per_arm * self.arms.len()
        }

        /// The samples file's head: how each input was derived.
        fn head(&self, host: &str) -> Result<String, String> {
            let boot = hex_runtime::guest::recorded_boot().map_err(|e| e.to_string())?;
            Ok(format!(
                "# hex-macos-cpu-response samples, host {host}\n\
                 # samples {}: {} arms taken {} times each, round-robin, at least SEEDABLE {} \
                 (bench/competitive/probes/waits_census.py, decision 9)\n\
                 # bound_nanos {}: the guest's recorded first dial bound over boot_ready {} ns of class {} \
                 (bench/native/guest/records.tsv; hex_runtime::guest::recorded_dial_bound), the managed \
                 helper's control patience\n\
                 # waits: from the watcher timer's resolution {} ns (NOTE_USECONDS) doubling below the bound, \
                 then the bound\n\
                 # wait_nanos sample_nanos\n",
                self.samples(),
                self.arms.len(),
                self.per_arm,
                self.seedable,
                self.bound.as_nanos(),
                boot.as_nanos(),
                hex_runtime::guest::CLASS,
                RESOLUTION.as_nanos(),
            ))
        }
    }

    /// A cost pair the row joins: the cost, its SHA-256 and the evidence its id hashes.
    struct Pair {
        cost_file: PathBuf,
        cost_sha256: [u8; 32],
        evidence: PathBuf,
    }

    struct Args {
        pair: Option<Pair>,
        out: PathBuf,
    }

    impl Args {
        /// `--out` alone, or with the pair's three options; each once.
        fn parse(words: &[String]) -> Result<Self, String> {
            let (mut cost_file, mut digits, mut evidence, mut out) = (None, None, None, None);
            let mut words = words.iter();
            while let Some(name) = words.next() {
                let slot = match name.as_str() {
                    "--cost-file" => &mut cost_file,
                    "--cost-sha256" => &mut digits,
                    "--evidence" => &mut evidence,
                    "--out" => &mut out,
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
            let out = PathBuf::from(out.ok_or_else(|| missing("--out"))?);
            let pair = match (cost_file, digits, evidence) {
                (None, None, None) => None,
                (Some(cost_file), Some(digits), Some(evidence)) => {
                    let mut cost_sha256 = [0u8; 32];
                    if digits.len() != 64 {
                        return Err("--cost-sha256 must be 64 hex digits".to_owned());
                    }
                    for (byte, at) in cost_sha256.iter_mut().zip((0..64).step_by(2)) {
                        *byte = u8::from_str_radix(digits.get(at..at + 2).unwrap_or("zz"), 16)
                            .map_err(|_| "--cost-sha256 must be 64 hex digits".to_owned())?;
                    }
                    Some(Pair {
                        cost_file: PathBuf::from(cost_file),
                        cost_sha256,
                        evidence: PathBuf::from(evidence),
                    })
                }
                _ => {
                    return Err(format!(
                        "--cost-file, --cost-sha256 and --evidence go together\n{USAGE}"
                    ));
                }
            };
            Ok(Self { pair, out })
        }
    }

    fn hex(bytes: &[u8]) -> String {
        bytes.iter().map(|b| format!("{b:02x}")).collect()
    }

    pub fn run() -> ExitCode {
        let words: Result<Vec<String>, String> = std::env::args_os()
            .skip(1)
            .map(|word| {
                word.into_string()
                    .map_err(|_| "arguments must be UTF-8".to_owned())
            })
            .collect();
        let run = words.and_then(|words| {
            let args = Args::parse(&words)?;
            let plan = Plan::derived()?;
            println!(
                "plan: {} samples over {} arms ({:?} to {:?}), {} each; SEEDABLE {}",
                plan.samples(),
                plan.arms.len(),
                RESOLUTION,
                plan.bound,
                plan.per_arm,
                plan.seedable
            );
            produce(&args, &plan)
        });
        match run {
            Ok(made) => {
                let row = &made.row;
                println!(
                    "host {} samples {} p50 {:?} p90 {:?} p99 {:?} max {:?}",
                    row.host, row.samples, row.p50, row.p90, row.p99, row.max
                );
                println!(
                    "samples {} sha256 {}",
                    made.samples_file.display(),
                    hex(&row.raw)
                );
                match &made.wrote {
                    Wrote::Row { file, sha256 } => println!(
                        "row file {} sha256 {}: guest_cost --cpu-response {} --cpu-response-sha256 {}",
                        file.display(),
                        hex(sha256),
                        file.display(),
                        hex(sha256)
                    ),
                    Wrote::Pair {
                        evidence_file,
                        cost_file,
                        cost_id,
                        cost_sha256,
                    } => {
                        println!(
                            "evidence {} (cost id {})",
                            evidence_file.display(),
                            hex(cost_id)
                        );
                        println!(
                            "cost {} HEX_GUEST_COST_SHA256={}",
                            cost_file.display(),
                            hex(cost_sha256)
                        );
                    }
                }
                ExitCode::SUCCESS
            }
            Err(why) => {
                eprintln!("macos_cpu_response: {why}");
                ExitCode::FAILURE
            }
        }
    }

    /// What a run wrote beside its samples: the row file, or the pair with the row at its evidence's head.
    enum Wrote {
        Row {
            file: PathBuf,
            sha256: [u8; 32],
        },
        Pair {
            evidence_file: PathBuf,
            cost_file: PathBuf,
            cost_id: [u8; 32],
            cost_sha256: [u8; 32],
        },
    }

    /// What a run wrote.
    struct Made {
        row: hex_runtime::guest::CpuResponse,
        samples_file: PathBuf,
        wrote: Wrote,
    }

    fn produce(args: &Args, plan: &Plan) -> Result<Made, String> {
        use sha2::{Digest, Sha256};

        // The pair's inputs as the node reads them, before any sample: the cost, its SHA-256 and the
        // evidence its id hashes.
        let pair = args
            .pair
            .as_ref()
            .map(|pair| -> Result<_, String> {
                let (cost, _, _) = hex_runtime::guest::load_runtime_evidence(
                    &pair.cost_file,
                    pair.cost_sha256,
                    &pair.evidence,
                )
                .map_err(|e| e.to_string())?;
                let evidence = std::fs::read(&pair.evidence).map_err(|e| e.to_string())?;
                Ok((cost, evidence))
            })
            .transpose()?;
        let host = hex_runtime::macos::machine_model().map_err(|e| e.to_string())?;
        let mut raw = plan.head(&host)?;

        let taken = sample(plan)?;
        for (wait, took) in &taken {
            raw.push_str(&format!("{} {}\n", wait.as_nanos(), took.as_nanos()));
        }
        let mut sorted: Vec<Duration> = taken.iter().map(|(_, took)| *took).collect();
        sorted.sort_unstable();
        // Nearest rank: the smallest sample with at least that share of samples at or below it.
        let rank = |share: usize| -> Duration {
            let at = (sorted.len() * share).div_ceil(100).max(1) - 1;
            sorted.get(at).copied().unwrap_or_default()
        };
        let row = hex_runtime::guest::CpuResponse {
            host: host.clone(),
            samples: u64::try_from(sorted.len()).map_err(|e| e.to_string())?,
            p50: rank(50),
            p90: rank(90),
            p99: rank(99),
            max: sorted.last().copied().unwrap_or_default(),
            raw: Sha256::digest(raw.as_bytes()).into(),
        };
        std::fs::create_dir_all(&args.out).map_err(|e| e.to_string())?;
        let samples_file = args.out.join(format!("cpu-response-{host}.samples"));
        std::fs::write(&samples_file, raw.as_bytes()).map_err(|e| e.to_string())?;
        let wrote = match pair {
            None => {
                let section = hex_runtime::guest::cpu_response_section(std::slice::from_ref(&row))
                    .map_err(|e| e.to_string())?;
                let file = args.out.join(format!("cpu-response-{host}.row"));
                std::fs::write(&file, &section).map_err(|e| e.to_string())?;
                Wrote::Row {
                    file,
                    sha256: Sha256::digest(&section).into(),
                }
            }
            Some((cost, evidence)) => {
                let (mut rows, original) =
                    hex_runtime::guest::cpu_response_split(&evidence).map_err(|e| e.to_string())?;
                rows.retain(|known| known.host != host);
                rows.push(row.clone());
                rows.sort_by(|a, b| a.host.cmp(&b.host));
                let mut written =
                    hex_runtime::guest::cpu_response_section(&rows).map_err(|e| e.to_string())?;
                written.extend_from_slice(original);
                let mut rekeyed = cost;
                rekeyed.id = Sha256::digest(&written).into();
                let cost_bytes = serde_json::to_vec(&rekeyed).map_err(|e| e.to_string())?;
                let (evidence_file, cost_file) =
                    (args.out.join("evidence"), args.out.join("cost.json"));
                std::fs::write(&evidence_file, &written).map_err(|e| e.to_string())?;
                std::fs::write(&cost_file, &cost_bytes).map_err(|e| e.to_string())?;
                Wrote::Pair {
                    evidence_file,
                    cost_file,
                    cost_id: rekeyed.id,
                    cost_sha256: Sha256::digest(&cost_bytes).into(),
                }
            }
        };
        Ok(Made {
            row,
            samples_file,
            wrote,
        })
    }

    /// Each arm `plan.per_arm` times, round-robin, on a busy member: its wait and the time from the timer's
    /// due instant to the member seen stopped.
    fn sample(plan: &Plan) -> Result<Vec<(Duration, Duration)>, String> {
        use hex_runtime::macos::sys::{self, Event, Kqueue, Run};
        use rustix::process::{Pid, Signal, kill_process};
        use std::time::Instant;

        let mut member = std::process::Command::new("/usr/bin/yes")
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .map_err(|e| format!("starting the busy member: {e}"))?;
        let pid = i32::try_from(member.id()).map_err(|e| e.to_string())?;
        let target = Pid::from_raw(pid).ok_or("member pid")?;
        let kq = Kqueue::new().map_err(|e| e.to_string())?;
        let mut events = Vec::new();
        let mut taken = Vec::with_capacity(plan.samples());
        let result = (|| -> Result<(), String> {
            for _ in 0..plan.per_arm {
                for &wait in &plan.arms {
                    let due = Instant::now() + wait;
                    let deadline = due + plan.bound;
                    let mut timer = Some(wait);
                    events.clear();
                    // The timer's own event ends the wait, never an interrupted kevent's empty return.
                    while !events.iter().any(|event| matches!(event, Event::Timer)) {
                        let left = deadline
                            .checked_duration_since(Instant::now())
                            .ok_or_else(|| format!("the timer armed for {wait:?} never fired"))?;
                        kq.wait(timer.take(), Some(left), &mut events)
                            .map_err(|e| format!("the watcher timer: {e}"))?;
                    }
                    kill_process(target, Signal::STOP)
                        .map_err(|e| format!("stopping the member: {e}"))?;
                    while sys::run_state(pid).map_err(|e| e.to_string())? != Some(Run::Stopped) {
                        if Instant::now() > deadline {
                            return Err(format!("no stop seen within the bound {:?}", plan.bound));
                        }
                        std::thread::yield_now();
                    }
                    taken.push((wait, Instant::now().saturating_duration_since(due)));
                    kill_process(target, Signal::CONT)
                        .map_err(|e| format!("continuing the member: {e}"))?;
                }
            }
            Ok(())
        })();
        let _ = member.kill();
        let _ = member.wait();
        result.map(|()| taken)
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        /// AC-T7.4.1: derive the producer's plan: expect the census's SEEDABLE, the guest's recorded first dial
        /// bound, arms from the timer's resolution doubling below that bound and then the bound, and each arm
        /// taken equally, together at least SEEDABLE and short of one more round.
        #[test]
        fn the_plan_is_derived_from_the_census_the_record_and_the_timer() {
            let plan = Plan::derived().unwrap();
            assert_eq!(plan.seedable, 1_000);
            assert_eq!(
                plan.bound,
                hex_runtime::guest::recorded_dial_bound().unwrap()
            );
            assert_eq!(plan.arms.first(), Some(&RESOLUTION));
            assert_eq!(plan.arms.last(), Some(&plan.bound));
            for pair in plan.arms.windows(2) {
                assert!(pair[0] < pair[1] && pair[1] <= pair[0] * 2, "{pair:?}");
                assert!(pair[1] == pair[0] * 2 || pair[1] == plan.bound, "{pair:?}");
            }
            assert!(plan.samples() >= plan.seedable, "{}", plan.samples());
            assert!(plan.samples() < plan.seedable + plan.arms.len());
            assert!(Args::parse(&["--wait-nanos".to_owned(), "1".to_owned()]).is_err());
        }

        /// AC-T7.4.1: run the producer once over every arm of the derived plan, on a cost and its evidence:
        /// expect the samples file to hold the plan's head and each sample beside its arm, in arm order; the
        /// evidence's row for this host to count and hash them, ahead of the original evidence; and the
        /// re-keyed cost to load with that row as the node loads it.
        #[test]
        fn one_round_writes_its_samples_its_row_and_the_rekeyed_cost() {
            use sha2::{Digest, Sha256};
            let dir = tempfile::tempdir().unwrap();
            let original = b"measured\n";
            let evidence = dir.path().join("evidence.in");
            std::fs::write(&evidence, original).unwrap();
            let fixed = hex_api::CapacityResources {
                cpu_milli: 1000,
                mem_bytes: shards_api::min_memory_mib(std::env::consts::ARCH) << 20,
                disk_bytes: 0,
            };
            let cost = hex_api::RuntimeCost {
                id: Sha256::digest(original).into(),
                floor: fixed,
                ceiling: fixed,
                quantum: hex_api::CapacityResources {
                    cpu_milli: 1,
                    mem_bytes: 1,
                    disk_bytes: 1,
                },
                overhead: hex_api::CapacityResources::default(),
                guest_overhead: hex_api::CapacityResources::default(),
            };
            let cost_bytes = serde_json::to_vec(&cost).unwrap();
            let cost_file = dir.path().join("cost.in.json");
            std::fs::write(&cost_file, &cost_bytes).unwrap();
            let args = Args::parse(&[
                "--cost-file".to_owned(),
                cost_file.display().to_string(),
                "--cost-sha256".to_owned(),
                hex(&Sha256::digest(&cost_bytes)),
                "--evidence".to_owned(),
                evidence.display().to_string(),
                "--out".to_owned(),
                dir.path().join("out").display().to_string(),
            ])
            .unwrap();
            let derived = Plan::derived().unwrap();
            let plan = Plan::new(1, derived.bound).unwrap();
            assert_eq!(plan.arms, derived.arms);

            let made = produce(&args, &plan).unwrap();
            let samples = std::fs::read(&made.samples_file).unwrap();
            let text = std::str::from_utf8(&samples).unwrap();
            assert!(text.starts_with(&plan.head(&made.row.host).unwrap()));
            let waits: Vec<Duration> = text
                .lines()
                .filter(|line| !line.starts_with('#'))
                .map(|line| {
                    let (wait, took) = line.split_once(' ').unwrap();
                    assert!(took.parse::<u64>().unwrap() > 0, "{line}");
                    Duration::from_nanos(wait.parse().unwrap())
                })
                .collect();
            assert_eq!(waits, plan.arms);
            let digest: [u8; 32] = Sha256::digest(&samples).into();
            assert_eq!(made.row.raw, digest);
            assert_eq!(made.row.samples, u64::try_from(plan.arms.len()).unwrap());
            assert_eq!(made.row.host, hex_runtime::macos::machine_model().unwrap());

            let Wrote::Pair {
                evidence_file,
                cost_file,
                cost_id,
                cost_sha256,
            } = &made.wrote
            else {
                panic!("a pair's run wrote no pair");
            };
            let written = std::fs::read(evidence_file).unwrap();
            let (rows, rest) = hex_runtime::guest::cpu_response_split(&written).unwrap();
            assert_eq!(rest, original);
            assert_eq!(rows, vec![made.row.clone()]);
            let (loaded, _, responses) =
                hex_runtime::guest::load_runtime_evidence(cost_file, *cost_sha256, evidence_file)
                    .unwrap();
            assert_eq!(&loaded.id, cost_id);
            assert_eq!(responses, vec![made.row]);
        }

        /// AC-T7.4.1 (ROOT's Mac order: the row, then guest_cost): run the producer with `--out` alone, once
        /// over every arm: expect a row file of its own, the CPU-response section holding this host's one row
        /// and nothing after it, its SHA-256 the one reported, the row hashing its samples; and the pair's
        /// options given in part refused.
        #[test]
        fn out_alone_writes_the_row_as_a_file_of_its_own() {
            use sha2::{Digest, Sha256};
            let dir = tempfile::tempdir().unwrap();
            let out = dir.path().join("out");
            let args = Args::parse(&["--out".to_owned(), out.display().to_string()]).unwrap();
            assert!(args.pair.is_none());
            let plan = Plan::new(1, Plan::derived().unwrap().bound).unwrap();
            let made = produce(&args, &plan).unwrap();
            let Wrote::Row { file, sha256 } = &made.wrote else {
                panic!("--out alone wrote a pair");
            };
            let bytes = std::fs::read(file).unwrap();
            let digest: [u8; 32] = Sha256::digest(&bytes).into();
            assert_eq!(&digest, sha256);
            let (rows, rest) = hex_runtime::guest::cpu_response_split(&bytes).unwrap();
            assert!(rest.is_empty());
            assert_eq!(rows, vec![made.row.clone()]);
            assert_eq!(made.row.host, hex_runtime::macos::machine_model().unwrap());
            let samples: [u8; 32] =
                Sha256::digest(std::fs::read(&made.samples_file).unwrap()).into();
            assert_eq!(made.row.raw, samples);
            assert!(!out.join("evidence").exists() && !out.join("cost.json").exists());
            assert!(
                Args::parse(&[
                    "--out".to_owned(),
                    "o".to_owned(),
                    "--evidence".to_owned(),
                    "e".to_owned()
                ])
                .is_err()
            );
        }
    }
}
