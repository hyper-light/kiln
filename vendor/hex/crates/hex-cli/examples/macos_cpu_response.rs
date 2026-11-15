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
//! - waits: the watcher arms its timer for whatever credit is left, so the arms run from the timer's own resolution
//!   up to the longest the control arms it for, its hold at the least rate with the slowest stop seen so far
//!   (`hex_runtime::macos::cpu_control_longest_hold`), one per binary order of magnitude of its microsecond count,
//!   then that hold: found in a first round, one sample each, as the slowest stop raises it, then each taken the
//!   same number of times, round-robin. The row's max covers every arm. No record bounds any of it (the review's
//!   C-9 (c));
//! - its waits follow their parties (decision 9): a timer's event and a member's stop each within W*, the slowest
//!   sample seen so far, and past it told once and waited for on, never refused at a bound; a member that ends
//!   refuses the run.
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

    /// A run's samples, derived as the head of this file says, never an argument: at least SEEDABLE of them.
    struct Plan {
        seedable: usize,
    }

    impl Plan {
        fn derived() -> Result<Self, String> {
            let seedable = CENSUS
                .lines()
                .find_map(|line| line.strip_prefix("SEEDABLE = "))
                .and_then(|n| n.trim().replace('_', "").parse::<usize>().ok())
                .ok_or("bench/competitive/probes/waits_census.py names no SEEDABLE")?;
            Self::new(seedable)
        }

        fn new(seedable: usize) -> Result<Self, String> {
            if seedable == 0 {
                return Err("no samples to take for SEEDABLE 0".to_owned());
            }
            Ok(Self { seedable })
        }
    }

    /// What a run took: its arms, found as its first round raised the control's longest hold, each taken `per_arm` times
    /// in all, and each sample, its arm and the time from the timer's due instant to the member seen stopped.
    struct Sampled {
        arms: Vec<Duration>,
        per_arm: usize,
        /// The slowest sample of the first round, the longest hold the last arm is.
        slowest: Duration,
        taken: Vec<(Duration, Duration)>,
    }

    impl Sampled {
        /// The samples file's head: how each input was derived.
        fn head(&self, plan: &Plan, host: &str) -> String {
            format!(
                "# hex-macos-cpu-response samples, host {host}\n\
                 # samples {}: {} arms taken {} times each, round-robin, at least SEEDABLE {} \
                 (bench/competitive/probes/waits_census.py, decision 9)\n\
                 # waits: from the watcher timer's resolution {} ns (NOTE_USECONDS) doubling while below the \
                 longest the control arms it for, its hold at the least rate with the slowest stop seen \
                 (hex_runtime::macos::cpu_control_longest_hold), then that hold, {} ns at the first round's \
                 slowest stop, {} ns; no record read\n\
                 # wait_nanos sample_nanos\n",
                self.taken.len(),
                self.arms.len(),
                self.per_arm,
                plan.seedable,
                RESOLUTION.as_nanos(),
                self.arms.last().copied().unwrap_or_default().as_nanos(),
                self.slowest.as_nanos(),
            )
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
                "plan: at least SEEDABLE {} samples, over arms from {:?} doubling to the control's longest hold",
                plan.seedable, RESOLUTION
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
        let sampled = sample(plan)?;
        let mut raw = sampled.head(plan, &host);
        for (wait, took) in &sampled.taken {
            raw.push_str(&format!("{} {}\n", wait.as_nanos(), took.as_nanos()));
        }
        let mut sorted: Vec<Duration> = sampled.taken.iter().map(|(_, took)| *took).collect();
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

    /// A sample's wait, from its timer's due instant to its member seen stopped: within W*, the slowest sample this run has
    /// seen, its answer; past it told once and waited for on (decision 9), never refused at a bound. No record seeds it.
    static STOPPED: hex_core::patience::Patience =
        hex_core::patience::Patience::measuring("macos_cpu_response.stopped");

    /// The run's samples on a busy member, each its wait and the time from the timer's due instant to the member seen
    /// stopped: a first round over arms from [`RESOLUTION`] doubling while below the control's longest hold at the
    /// slowest sample seen, then that hold, then rounds over those arms, round-robin, until at least SEEDABLE.
    fn sample(plan: &Plan) -> Result<Sampled, String> {
        let mut member = std::process::Command::new("/usr/bin/yes")
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .map_err(|e| format!("starting the busy member: {e}"))?;
        let result = (|| -> Result<Sampled, String> {
            let pid = i32::try_from(member.id()).map_err(|e| e.to_string())?;
            let mut once = Once::new(pid)?;
            let mut taken = Vec::new();
            let mut arms = Vec::new();
            let mut slowest = Duration::ZERO;
            let mut arm = RESOLUTION;
            loop {
                let took = once.take(arm)?;
                taken.push((arm, took));
                arms.push(arm);
                slowest = slowest.max(took);
                let longest = hex_runtime::macos::cpu_control_longest_hold(slowest)
                    .map_err(|e| e.to_string())?;
                if arm >= longest {
                    break;
                }
                arm = arm.saturating_mul(2).min(longest);
            }
            let per_arm = plan.seedable.div_ceil(arms.len());
            for _ in 1..per_arm {
                for &arm in &arms {
                    taken.push((arm, once.take(arm)?));
                }
            }
            Ok(Sampled {
                arms,
                per_arm,
                slowest,
                taken,
            })
        })();
        let _ = member.kill();
        let _ = member.wait();
        result
    }

    /// One sample's means: the member, by its pid, and the watcher's kqueue.
    struct Once {
        pid: i32,
        kq: hex_runtime::macos::sys::Kqueue,
        events: Vec<hex_runtime::macos::sys::Event>,
    }

    impl Once {
        fn new(pid: i32) -> Result<Self, String> {
            Ok(Self {
                pid,
                kq: hex_runtime::macos::sys::Kqueue::new().map_err(|e| e.to_string())?,
                events: Vec::new(),
            })
        }

        /// The timer armed for `wait`, and once it fires the member stopped and read until the kernel reports it stopped:
        /// the time from the timer's due instant to that. Past W* ([`STOPPED`]) told once, and waited for on.
        fn take(&mut self, wait: Duration) -> Result<Duration, String> {
            use hex_core::patience::Piece;
            use hex_runtime::macos::sys::{self, Event, Run};
            use rustix::process::{Pid, Signal, kill_process};
            use std::time::Instant;

            let pid = self.pid;
            let target = Pid::from_raw(pid).ok_or("member pid")?;
            let due = Instant::now() + wait;
            let w_star = STOPPED.deadline();
            let mut told = false;
            let tell = |told: &mut bool| {
                if let Some(w_star) = w_star
                    && !*told
                    && due.elapsed() > w_star
                {
                    STOPPED.ran_out(
                        w_star,
                        due.elapsed(),
                        &[
                            Piece::Kind("the busy member, pid "),
                            Piece::Pid(i64::from(pid)),
                            Piece::Kind(", its timer's event and its stop"),
                        ],
                    );
                    *told = true;
                }
            };
            let mut timer = Some(wait);
            self.events.clear();
            // The timer's own event ends the wait, never an interrupted kevent's empty return: within W* past its due
            // instant, then with no bound.
            while !self
                .events
                .iter()
                .any(|event| matches!(event, Event::Timer))
            {
                let slice = w_star
                    .filter(|_| !told)
                    .and_then(|w_star| (due + w_star).checked_duration_since(Instant::now()));
                self.kq
                    .wait(timer.take(), slice, &mut self.events)
                    .map_err(|e| format!("the watcher timer: {e}"))?;
                tell(&mut told);
            }
            kill_process(target, Signal::STOP).map_err(|e| format!("stopping the member: {e}"))?;
            loop {
                match sys::run_state(pid).map_err(|e| e.to_string())? {
                    Some(Run::Stopped) => break,
                    Some(_) => {}
                    None => return Err("the busy member ended before its stop was seen".to_owned()),
                }
                tell(&mut told);
                std::thread::yield_now();
            }
            let took = due.elapsed();
            STOPPED.answered(took);
            kill_process(target, Signal::CONT)
                .map_err(|e| format!("continuing the member: {e}"))?;
            Ok(took)
        }
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        /// AC-T7.4.1 (the review's C-9 (c): no record read): derive the producer's plan: expect the census's SEEDABLE
        /// alone, and no argument naming a wait.
        #[test]
        fn the_plan_is_derived_from_the_census() {
            let plan = Plan::derived().unwrap();
            assert_eq!(plan.seedable, 1_000);
            assert!(Plan::new(0).is_err());
            assert!(Args::parse(&["--wait-nanos".to_owned(), "1".to_owned()]).is_err());
        }

        /// A run's arms as its samples file holds them, in order: from the timer's resolution, each twice the one
        /// before, or less where the control's longest hold, rising with the slowest stop seen, is less.
        fn arms_of(text: &str) -> Vec<Duration> {
            let waits: Vec<Duration> = text
                .lines()
                .filter(|line| !line.starts_with('#'))
                .map(|line| {
                    let (wait, took) = line.split_once(' ').unwrap();
                    assert!(took.parse::<u64>().unwrap() > 0, "{line}");
                    Duration::from_nanos(wait.parse().unwrap())
                })
                .collect();
            assert_eq!(waits.first(), Some(&RESOLUTION));
            for pair in waits.windows(2) {
                assert!(pair[0] < pair[1] && pair[1] <= pair[0] * 2, "{pair:?}");
            }
            waits
        }

        /// AC-T7.4.1: run the producer's first round alone (SEEDABLE 1), on a cost and its evidence: expect the samples
        /// file to hold its head and each sample beside its arm, the arms from the timer's resolution doubling to the
        /// control's longest hold; the evidence's row for this host to count and hash them, ahead of the original
        /// evidence; and the re-keyed cost to load with that row as the node loads it.
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
            let plan = Plan::new(1).unwrap();

            let made = produce(&args, &plan).unwrap();
            let samples = std::fs::read(&made.samples_file).unwrap();
            let text = std::str::from_utf8(&samples).unwrap();
            assert!(text.starts_with(&format!(
                "# hex-macos-cpu-response samples, host {}\n",
                made.row.host
            )));
            let arms = arms_of(text);
            let digest: [u8; 32] = Sha256::digest(&samples).into();
            assert_eq!(made.row.raw, digest);
            assert_eq!(made.row.samples, u64::try_from(arms.len()).unwrap());
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

        /// AC-T7.4.1 (ROOT's Mac order: the row, then guest_cost): run the producer with `--out` alone, its first round
        /// alone: expect a row file of its own, the CPU-response section holding this host's one row
        /// and nothing after it, its SHA-256 the one reported, the row hashing its samples; and the pair's
        /// options given in part refused.
        #[test]
        fn out_alone_writes_the_row_as_a_file_of_its_own() {
            use sha2::{Digest, Sha256};
            let dir = tempfile::tempdir().unwrap();
            let out = dir.path().join("out");
            let args = Args::parse(&["--out".to_owned(), out.display().to_string()]).unwrap();
            assert!(args.pair.is_none());
            let plan = Plan::new(1).unwrap();
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
