//! A box's bounds as cgroup v2 file writes, and its usage read back from cgroup v2 files. File names,
//! formats and units are the kernel's (Documentation/admin-guide/cgroup-v2.rst). A cgroup handed from
//! one box to the next of its owner (goal 5 extended, step 6; [`crate::reuse`]) has every bound written
//! again ([`writes_all`]) and its usage reported from what it had used when the box was given it
//! ([`Baseline`], [`since`]).

use hex_api::{Bounds, Refusal, Usage};
use serde::{Deserialize, Serialize};

/// `cpu.max`'s period, in microseconds: the kernel's default (100 ms).
pub const CPU_PERIOD_US: u64 = 100_000;
/// `cpu.max` quota for each thousandth of a CPU: a thousandth of [`CPU_PERIOD_US`].
const QUOTA_US_PER_MILLI: u64 = 100;

/// The controllers a box's parent enables for it: those [`Bounds`] writes to.
pub const CONTROLLERS: [&str; 4] = ["cpu", "io", "memory", "pids"];

/// A block device, by number.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Dev {
    pub major: u32,
    pub minor: u32,
}

/// One write to a file in the box's cgroup directory.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Write {
    pub file: &'static str,
    pub value: String,
}

fn w(file: &'static str, value: String) -> Write {
    Write { file, value }
}

/// What to write so the box holds `bounds`: `disks` are the devices `io.latency` and `io.max` apply
/// to (the disks the node keeps boxes' files on). Writes come in the order the kernel needs: a box's
/// `memory.high` is never set above a `memory.max` written after it.
pub fn writes(bounds: &Bounds, disks: &[Dev]) -> Result<Vec<Write>, Refusal> {
    bounds.check()?;
    let mut out = Vec::new();
    if let Some(weight) = bounds.cpu_weight {
        out.push(w("cpu.weight", weight.to_string()));
    }
    if let Some(milli) = bounds.cpu_max_milli {
        let quota = u64::from(milli)
            .checked_mul(QUOTA_US_PER_MILLI)
            .ok_or(Refusal::Invalid {
                what: "cpu_max_milli",
                why: format!("{milli} overflows cpu.max"),
            })?;
        out.push(w("cpu.max", format!("{quota} {CPU_PERIOD_US}")));
    }
    if let Some(max) = bounds.memory_max {
        out.push(w("memory.max", max.to_string()));
        // The bound covers all of the box's memory: with swap open, a box past memory.max swaps
        // instead of being killed, so on a host with swap it is no bound (a 512 MiB hog finished
        // in a 64 MiB box on zram, 2026-10-23). memory.swap.max 0 keeps its anonymous memory in RAM.
        out.push(w("memory.swap.max", "0".to_owned()));
    }
    if let Some(high) = bounds.memory_high {
        out.push(w("memory.high", high.to_string()));
    }
    if let Some(weight) = bounds.io_weight {
        out.push(w("io.weight", format!("default {weight}")));
    }
    for d in disks {
        if let Some(target) = bounds.io_latency_us {
            out.push(w(
                "io.latency",
                format!("{}:{} target={target}", d.major, d.minor),
            ));
        }
        if let Some(r) = bounds.io_max {
            let rate = |v: Option<u64>| v.map_or_else(|| "max".to_owned(), |v| v.to_string());
            out.push(w(
                "io.max",
                format!(
                    "{}:{} rbps={} wbps={} riops={} wiops={}",
                    d.major,
                    d.minor,
                    rate(r.read_bps),
                    rate(r.write_bps),
                    rate(r.read_iops),
                    rate(r.write_iops)
                ),
            ));
        }
    }
    if let Some(max) = bounds.pids_max {
        out.push(w("pids.max", max.to_string()));
    }
    Ok(out)
}

/// What to write so a cgroup an earlier box left holds `bounds` and nothing of the earlier box's: every
/// file [`writes`] can write, each bound `bounds` doesn't set at the kernel's default for its file
/// (Documentation/admin-guide/cgroup-v2.rst v6.19: `cpu.weight` 100, `cpu.max` "max 100000",
/// `memory.max`, `memory.swap.max`, `memory.high` and `pids.max` "max", `io.weight` "default 100",
/// `io.max`'s rates "max"; `io.latency`'s "target=max" turns it off, block/blk-iolatency.c:873-874).
/// In the order [`writes`] keeps. A file the kernel doesn't have (a controller not handed down, a kernel
/// without swap accounting) holds no earlier box's value either, so the shell skips a default it can't
/// open; a bound the box sets is refused, as [`writes`]'s is, before anything is made.
pub fn writes_all(bounds: &Bounds, disks: &[Dev]) -> Result<Vec<Write>, Refusal> {
    bounds.check()?;
    let set = writes(bounds, disks)?;
    let given = |file: &str, disk: Option<Dev>| {
        set.iter()
            .find(|w| {
                w.file == file
                    && disk
                        .is_none_or(|d| w.value.starts_with(&format!("{}:{} ", d.major, d.minor)))
            })
            .map(|w| w.value.clone())
    };
    let mut out = vec![
        w(
            "cpu.weight",
            given("cpu.weight", None).unwrap_or_else(|| "100".to_owned()),
        ),
        w(
            "cpu.max",
            given("cpu.max", None).unwrap_or_else(|| format!("max {CPU_PERIOD_US}")),
        ),
        w(
            "memory.max",
            given("memory.max", None).unwrap_or_else(|| "max".to_owned()),
        ),
        w(
            "memory.swap.max",
            given("memory.swap.max", None).unwrap_or_else(|| "max".to_owned()),
        ),
        w(
            "memory.high",
            given("memory.high", None).unwrap_or_else(|| "max".to_owned()),
        ),
        w(
            "io.weight",
            given("io.weight", None).unwrap_or_else(|| "default 100".to_owned()),
        ),
    ];
    for d in disks {
        let dev = format!("{}:{}", d.major, d.minor);
        out.push(w(
            "io.latency",
            given("io.latency", Some(*d)).unwrap_or_else(|| format!("{dev} target=max")),
        ));
        out.push(w(
            "io.max",
            given("io.max", Some(*d))
                .unwrap_or_else(|| format!("{dev} rbps=max wbps=max riops=max wiops=max")),
        ));
    }
    out.push(w(
        "pids.max",
        given("pids.max", None).unwrap_or_else(|| "max".to_owned()),
    ));
    Ok(out)
}

/// Whether `write` sets a file to the kernel's default for it rather than to a bound a box asked for:
/// such a write may be skipped where the file isn't there ([`writes_all`]).
pub fn is_default(write: &Write) -> bool {
    let v = write.value.as_str();
    match write.file {
        "cpu.weight" => v == "100",
        "cpu.max" => v == format!("max {CPU_PERIOD_US}"),
        "io.weight" => v == "default 100",
        "io.latency" => v.ends_with(" target=max"),
        "io.max" => v.ends_with(" rbps=max wbps=max riops=max wiops=max"),
        _ => v == "max",
    }
}

/// `cpu.max`'s two values as the kernel reads and writes them ("$MAX $PERIOD",
/// Documentation/admin-guide/cgroup-v2.rst v6.19): the quota each period may use, `None` for `max`, and
/// the period, in microseconds. A reused cgroup's `cpu.max` is written again only when these differ from
/// the box's, since its write takes the host-wide `cfs_constraints_mutex` and walks every task group
/// (Linux v6.19 kernel/sched/core.c:9461-9462, :9624).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CpuMax {
    /// The quota each period may use; `None` for `max`.
    pub quota_us: Option<u64>,
    /// The period.
    pub period_us: u64,
}

impl CpuMax {
    /// The two values `text` holds, or `None` unless it is exactly a quota (`max` or a number) and a
    /// period (a number), spaced however.
    pub fn parse(text: &str) -> Option<Self> {
        let mut fields = text.split_ascii_whitespace();
        let (quota, period) = (fields.next()?, fields.next()?);
        if fields.next().is_some() {
            return None;
        }
        let quota_us = match quota {
            "max" => None,
            n => Some(n.parse().ok()?),
        };
        Some(Self {
            quota_us,
            period_us: period.parse().ok()?,
        })
    }
}

/// A cgroup's cumulative counters when a box was given it. A cgroup's counters run on from the boxes
/// before it and none can be reset (research f32, f33), so a box given a cgroup another box left has its
/// usage reported from these ([`since`]). Kept in the box's record, so a restarted node reports the same.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Baseline {
    pub cpu_usec: u64,
    pub io_read_bytes: u64,
    pub io_write_bytes: u64,
    pub oom_kills: u64,
    pub pids_refused: u64,
}

impl Baseline {
    /// The counters of `usage`, read from the cgroup as it's handed on.
    pub fn of(usage: &Usage) -> Self {
        Self {
            cpu_usec: usage.cpu_usec,
            io_read_bytes: usage.io_read_bytes,
            io_write_bytes: usage.io_write_bytes,
            oom_kills: usage.oom_kills,
            pids_refused: usage.pids_refused,
        }
    }
}

/// What a box given a cgroup at `base` has used, from what the cgroup says now (`now`): each counter
/// less the baseline's, its memory and processes as they are, and its peak as `peak` says, read through
/// a `memory.peak` descriptor written when the box was given the cgroup, which reads the peak since that
/// write alone (Linux 6.12 on; research f31), never the cgroup's lifetime peak. A counter under its
/// baseline (it can't fall) reads 0. `pids.peak` never resets (f32), so a box's usage carries none.
pub fn since(now: &Usage, base: &Baseline, peak: u64) -> Usage {
    Usage {
        memory_bytes: now.memory_bytes,
        memory_peak_bytes: peak,
        oom_kills: now.oom_kills.saturating_sub(base.oom_kills),
        pids: now.pids,
        pids_refused: now.pids_refused.saturating_sub(base.pids_refused),
        cpu_usec: now.cpu_usec.saturating_sub(base.cpu_usec),
        io_read_bytes: now.io_read_bytes.saturating_sub(base.io_read_bytes),
        io_write_bytes: now.io_write_bytes.saturating_sub(base.io_write_bytes),
        guest: None,
    }
}

/// The cgroup files [`usage`] reads, by name; a file the kernel doesn't have reads as empty.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct UsageFiles {
    pub memory_current: String,
    pub memory_peak: String,
    pub memory_events: String,
    pub pids_current: String,
    pub pids_events: String,
    pub cpu_stat: String,
    pub io_stat: String,
}

/// What the files say the box used. A field the kernel didn't report reads 0; a value that isn't a
/// number is skipped, never trusted.
pub fn usage(f: &UsageFiles) -> Usage {
    let num = |s: &str| s.trim().parse::<u64>().unwrap_or(0);
    let mut io_read_bytes = 0u64;
    let mut io_write_bytes = 0u64;
    for line in f.io_stat.lines() {
        io_read_bytes = io_read_bytes.saturating_add(keyed(line, "rbytes=").unwrap_or(0));
        io_write_bytes = io_write_bytes.saturating_add(keyed(line, "wbytes=").unwrap_or(0));
    }
    Usage {
        memory_bytes: num(&f.memory_current),
        memory_peak_bytes: num(&f.memory_peak),
        oom_kills: flat(&f.memory_events, "oom_kill").unwrap_or(0),
        pids: num(&f.pids_current),
        pids_refused: flat(&f.pids_events, "max").unwrap_or(0),
        cpu_usec: flat(&f.cpu_stat, "usage_usec").unwrap_or(0),
        io_read_bytes,
        io_write_bytes,
        guest: None,
    }
}

/// Native backing accounting requires every counter used by [`Usage`]. Unlike [`usage`], missing or
/// malformed counters cannot become room for another allocation. The shell must have successfully
/// read all seven files from the same pinned cgroup; an empty `io.stat` is a valid read of no I/O.
pub fn managed_usage(f: &UsageFiles) -> Result<Usage, Refusal> {
    managed_number(&f.memory_current, "memory.current")?;
    managed_number(&f.memory_peak, "memory.peak")?;
    managed_number(&f.pids_current, "pids.current")?;
    managed_flat(&f.memory_events, "oom_kill", "memory.events")?;
    managed_flat(&f.pids_events, "max", "pids.events")?;
    managed_flat(&f.cpu_stat, "usage_usec", "cpu.stat")?;

    let mut read = 0u64;
    let mut written = 0u64;
    for line in f.io_stat.lines().filter(|line| !line.trim().is_empty()) {
        let mut parts = line.split_ascii_whitespace();
        let device = parts
            .next()
            .and_then(|value| value.split_once(':'))
            .ok_or_else(|| managed_invalid("io.stat", "missing major:minor device key"))?;
        for number in [device.0, device.1] {
            u32::try_from(managed_number(number, "io.stat")?)
                .map_err(|_| managed_invalid("io.stat", "device number exceeds u32"))?;
        }
        let mut rbytes = None;
        let mut wbytes = None;
        for field in parts {
            let (key, value) = field
                .split_once('=')
                .filter(|(key, _)| !key.is_empty())
                .ok_or_else(|| managed_invalid("io.stat", "invalid nested counter"))?;
            managed_number(value, "io.stat")?;
            let target = match key {
                "rbytes" => &mut rbytes,
                "wbytes" => &mut wbytes,
                _ => continue,
            };
            if target.replace(value).is_some() {
                return Err(managed_invalid("io.stat", "duplicate required counter"));
            }
        }
        let rbytes = rbytes.ok_or_else(|| managed_invalid("io.stat", "missing rbytes counter"))?;
        let wbytes = wbytes.ok_or_else(|| managed_invalid("io.stat", "missing wbytes counter"))?;
        // The ordinary parser saturates. That cannot hide an overflow in measured backing usage.
        read = read
            .checked_add(managed_number(rbytes, "io.stat")?)
            .ok_or_else(|| managed_invalid("io.stat", "read-byte sum overflows u64"))?;
        written = written
            .checked_add(managed_number(wbytes, "io.stat")?)
            .ok_or_else(|| managed_invalid("io.stat", "write-byte sum overflows u64"))?;
    }
    Ok(usage(f))
}

fn managed_number(text: &str, file: &'static str) -> Result<u64, Refusal> {
    let text = text.trim();
    if text.is_empty() || !text.bytes().all(|byte| byte.is_ascii_digit()) {
        return Err(managed_invalid(
            file,
            "required counter is not an unsigned decimal",
        ));
    }
    text.parse()
        .map_err(|_| managed_invalid(file, "required counter exceeds u64"))
}

fn managed_flat(text: &str, required: &str, file: &'static str) -> Result<(), Refusal> {
    let mut found = false;
    for line in text.lines().filter(|line| !line.trim().is_empty()) {
        let mut parts = line.split_ascii_whitespace();
        let key = parts
            .next()
            .ok_or_else(|| managed_invalid(file, "missing counter key"))?;
        let value = parts
            .next()
            .ok_or_else(|| managed_invalid(file, "missing counter value"))?;
        managed_number(value, file)?;
        if parts.next().is_some() {
            return Err(managed_invalid(file, "extra counter fields"));
        }
        if key == required {
            if found {
                return Err(managed_invalid(file, "duplicate required counter"));
            }
            found = true;
        }
    }
    if !found {
        return Err(managed_invalid(file, "missing required counter"));
    }
    Ok(())
}

fn managed_invalid(file: &'static str, message: &'static str) -> Refusal {
    let mut why = String::new();
    if why.try_reserve_exact(message.len()).is_err() {
        return Refusal::Exhausted {
            what: "managed usage refusal bytes",
            bound: message.len(),
        };
    }
    why.push_str(message);
    Refusal::Invalid { what: file, why }
}

#[cfg(test)]
mod managed_usage_tests {
    use super::*;

    fn counters() -> UsageFiles {
        UsageFiles {
            memory_current: "2048\n".into(),
            memory_peak: "4096\n".into(),
            memory_events: "low 0\noom_kill 2\noom_group_kill 0\n".into(),
            pids_current: "3\n".into(),
            pids_events: "max 4\n".into(),
            cpu_stat: "system_usec 20\nusage_usec 70\nuser_usec 50\n".into(),
            io_stat: "8:16 wbytes=200 rbytes=100 rios=1\n8:0 rbytes=7 wbytes=11\n".into(),
        }
    }

    /// AC-T7.1.2: actual backing usage has all kernel-reported fields; a valid empty I/O report
    /// measures zero while unknown well-formed counters and kernel ordering remain compatible.
    #[test]
    fn strict_usage_accepts_complete_counters_and_empty_io() {
        let mut files = counters();
        assert_eq!(
            managed_usage(&files),
            Ok(Usage {
                memory_bytes: 2048,
                memory_peak_bytes: 4096,
                oom_kills: 2,
                pids: 3,
                pids_refused: 4,
                cpu_usec: 70,
                io_read_bytes: 107,
                io_write_bytes: 211,
                guest: None,
            })
        );
        files.io_stat.clear();
        let got = managed_usage(&files).unwrap();
        assert_eq!((got.io_read_bytes, got.io_write_bytes), (0, 0));
        files.memory_current = "0\n".into();
        files.memory_peak = "0\n".into();
        files.memory_events = "oom_kill 0\n".into();
        files.pids_current = "0\n".into();
        files.pids_events = "max 0\n".into();
        files.cpu_stat = "usage_usec 0\n".into();
        assert_eq!(managed_usage(&files), Ok(Usage::default()));
    }

    /// AC-T7.1.2: a missing or malformed required counter refuses the entire observation; it must
    /// never turn into spare capacity while the native backing remains charged.
    #[test]
    fn strict_usage_refuses_every_missing_or_malformed_required_counter() {
        for field in 0..7 {
            for damaged in ["", "not-a-counter", "-1", "18446744073709551616"] {
                if field == 6 && damaged.is_empty() {
                    continue; // A successfully read empty io.stat is valid.
                }
                let mut files = counters();
                let target = match field {
                    0 => &mut files.memory_current,
                    1 => &mut files.memory_peak,
                    2 => &mut files.memory_events,
                    3 => &mut files.pids_current,
                    4 => &mut files.pids_events,
                    5 => &mut files.cpu_stat,
                    6 => &mut files.io_stat,
                    _ => panic!("required-counter fixture index outside 0..7: {field}"),
                };
                *target = damaged.into();
                assert!(managed_usage(&files).is_err(), "field {field}: {damaged:?}");
            }
        }
        let mut files = counters();
        files.cpu_stat = "user_usec 50\nsystem_usec 20\n".into();
        assert!(matches!(
            managed_usage(&files),
            Err(Refusal::Invalid {
                what: "cpu.stat",
                ..
            })
        ));
    }

    /// AC-T7.1.2: hostile counter text cannot select one of conflicting facts or saturate an
    /// overflowing sum into a successful measurement.
    #[test]
    fn strict_usage_refuses_ambiguous_nested_and_flat_counters() {
        for cpu in [
            "usage_usec 1\nusage_usec 2\n",
            "usage_usec 1 extra\n",
            "usage_usec +1\n",
        ] {
            let mut files = counters();
            files.cpu_stat = cpu.into();
            assert!(managed_usage(&files).is_err());
        }
        for io in [
            "8:0 rbytes=1",
            "8:0 wbytes=1",
            "8:0 rbytes=1 wbytes=2 rbytes=3",
            "8:0 rbytes=1 wbytes=+2",
            "8:0 rbytes=1 wbytes=2 unknown=broken",
            "8:0:0 rbytes=1 wbytes=2",
            "4294967296:0 rbytes=1 wbytes=2",
            "8:0 rbytes=18446744073709551615 wbytes=0\n8:1 rbytes=1 wbytes=0",
            "8:0 rbytes=0 wbytes=18446744073709551615\n8:1 rbytes=0 wbytes=1",
        ] {
            let mut files = counters();
            files.io_stat = io.into();
            assert!(managed_usage(&files).is_err(), "{io:?}");
        }
    }
}

/// The value of `key` in a flat-keyed file ("key value" lines).
pub fn flat(text: &str, key: &str) -> Option<u64> {
    text.lines().find_map(|line| {
        let mut parts = line.split_ascii_whitespace();
        (parts.next() == Some(key))
            .then(|| parts.next().and_then(|v| v.parse().ok()))
            .flatten()
    })
}

/// The value of a `key=value` field in a nested-keyed line ("8:0 rbytes=1 wbytes=2").
fn keyed(line: &str, key: &str) -> Option<u64> {
    line.split_ascii_whitespace()
        .find_map(|f| f.strip_prefix(key).and_then(|v| v.parse().ok()))
}

/// Whether a cgroup's `cgroup.events` says processes run in it or below it.
pub fn populated(events: &str) -> Option<bool> {
    flat(events, "populated").map(|n| n != 0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use hex_api::IoRates;
    use proptest::prelude::*;

    #[test]
    fn every_bound_becomes_its_kernel_file_in_kernel_units() {
        let b = Bounds {
            cpu_weight: Some(50),
            cpu_max_milli: Some(1500),
            memory_max: Some(2 << 30),
            memory_high: Some(1 << 30),
            io_weight: Some(25),
            io_latency_us: Some(5000),
            io_max: Some(IoRates {
                write_bps: Some(100 << 20),
                ..IoRates::default()
            }),
            pids_max: Some(4096),
        };
        let disks = [
            Dev {
                major: 259,
                minor: 0,
            },
            Dev {
                major: 8,
                minor: 16,
            },
        ];
        let got: Vec<(&str, String)> = writes(&b, &disks)
            .unwrap()
            .into_iter()
            .map(|w| (w.file, w.value))
            .collect();
        let want: Vec<(&str, String)> = vec![
            ("cpu.weight", "50".into()),
            ("cpu.max", "150000 100000".into()),
            ("memory.max", "2147483648".into()),
            ("memory.swap.max", "0".into()),
            ("memory.high", "1073741824".into()),
            ("io.weight", "default 25".into()),
            ("io.latency", "259:0 target=5000".into()),
            (
                "io.max",
                "259:0 rbps=max wbps=104857600 riops=max wiops=max".into(),
            ),
            ("io.latency", "8:16 target=5000".into()),
            (
                "io.max",
                "8:16 rbps=max wbps=104857600 riops=max wiops=max".into(),
            ),
            ("pids.max", "4096".into()),
        ];
        assert_eq!(got, want);
    }

    #[test]
    fn no_bounds_write_nothing_and_an_invalid_bound_writes_nothing_either() {
        assert_eq!(
            writes(&Bounds::default(), &[Dev { major: 8, minor: 0 }]).unwrap(),
            vec![]
        );
        assert!(
            writes(
                &Bounds {
                    pids_max: Some(0),
                    ..Bounds::default()
                },
                &[]
            )
            .is_err()
        );
    }

    #[test]
    fn usage_reads_the_kernels_files() {
        let f = UsageFiles {
            memory_current: "1048576\n".into(),
            memory_peak: "4194304\n".into(),
            memory_events: "low 0\nhigh 3\nmax 7\noom 2\noom_kill 2\noom_group_kill 0\n".into(),
            pids_current: "5\n".into(),
            pids_events: "max 9\n".into(),
            cpu_stat: "usage_usec 123456\nuser_usec 100000\nsystem_usec 23456\n".into(),
            io_stat: "259:0 rbytes=100 wbytes=200 rios=1 wios=2 dbytes=0 dios=0\n8:16 rbytes=1 wbytes=2 rios=1 wios=1 dbytes=0 dios=0\n".into(),
        };
        let u = usage(&f);
        assert_eq!(
            u,
            Usage {
                memory_bytes: 1 << 20,
                memory_peak_bytes: 4 << 20,
                oom_kills: 2,
                pids: 5,
                pids_refused: 9,
                cpu_usec: 123_456,
                io_read_bytes: 101,
                io_write_bytes: 202,
                guest: None,
            }
        );
        assert_eq!(usage(&UsageFiles::default()), Usage::default());
        assert_eq!(populated("populated 1\nfrozen 0\n"), Some(true));
        assert_eq!(populated("populated 0\nfrozen 0\n"), Some(false));
        assert_eq!(populated("garbage"), None);
    }

    /// Goal 5 extended, step 6: a cgroup handed on gets every bound again, those the next box doesn't
    /// set at the kernel's defaults, so nothing of the last box's bounds stays in force.
    #[test]
    fn every_bound_is_written_again_with_the_kernels_default_for_those_unset() {
        let disks = [Dev { major: 8, minor: 0 }];
        let got: Vec<(&str, String)> = writes_all(&Bounds::default(), &disks)
            .unwrap()
            .into_iter()
            .map(|w| (w.file, w.value))
            .collect();
        let want: Vec<(&str, String)> = vec![
            ("cpu.weight", "100".into()),
            ("cpu.max", "max 100000".into()),
            ("memory.max", "max".into()),
            ("memory.swap.max", "max".into()),
            ("memory.high", "max".into()),
            ("io.weight", "default 100".into()),
            ("io.latency", "8:0 target=max".into()),
            ("io.max", "8:0 rbps=max wbps=max riops=max wiops=max".into()),
            ("pids.max", "max".into()),
        ];
        assert_eq!(got, want);
        assert!(
            writes_all(&Bounds::default(), &disks)
                .unwrap()
                .iter()
                .all(is_default)
        );
        let b = Bounds {
            memory_max: Some(64 << 20),
            pids_max: Some(16),
            io_latency_us: Some(5000),
            ..Bounds::default()
        };
        let set: Vec<(&str, String)> = writes_all(&b, &disks)
            .unwrap()
            .into_iter()
            .filter(|w| !is_default(w))
            .map(|w| (w.file, w.value))
            .collect();
        assert_eq!(
            set,
            vec![
                ("memory.max", "67108864".into()),
                ("memory.swap.max", "0".into()),
                ("io.latency", "8:0 target=5000".into()),
                ("pids.max", "16".into()),
            ]
        );
        assert!(
            writes_all(
                &Bounds {
                    pids_max: Some(0),
                    ..Bounds::default()
                },
                &disks
            )
            .is_err()
        );
    }

    /// Goal 5 extended, step 6 (research f31-f33): a box given a cgroup another box left is reported
    /// what it used since: the counters less the baseline read as it was given the cgroup, its peak as
    /// the descriptor written then reads it.
    #[test]
    fn a_handed_on_cgroups_usage_is_reported_from_its_baseline() {
        let before = Usage {
            memory_bytes: 500,
            memory_peak_bytes: 64 << 20,
            oom_kills: 2,
            pids: 0,
            pids_refused: 7,
            cpu_usec: 1_000_000,
            io_read_bytes: 10,
            io_write_bytes: 20,
            guest: None,
        };
        let base = Baseline::of(&before);
        let now = Usage {
            memory_bytes: 4096,
            memory_peak_bytes: 64 << 20,
            oom_kills: 2,
            pids: 1,
            pids_refused: 7,
            cpu_usec: 1_000_300,
            io_read_bytes: 15,
            io_write_bytes: 20,
            guest: None,
        };
        assert_eq!(
            since(&now, &base, 8192),
            Usage {
                memory_bytes: 4096,
                memory_peak_bytes: 8192,
                oom_kills: 0,
                pids: 1,
                pids_refused: 0,
                cpu_usec: 300,
                io_read_bytes: 5,
                io_write_bytes: 0,
                guest: None,
            }
        );
        // A counter read under its baseline, as a wrong baseline would give, reads 0, not a wrap.
        assert_eq!(since(&Usage::default(), &base, 0).cpu_usec, 0);
    }

    /// Goal 5 extended, step 6 (the lead's ruling on cpu.max, 2026-11-03T01:19Z): a reused cgroup's
    /// cpu.max is written again unless its parsed values already equal the box's; read a value either side
    /// can't parse as not equal, so the bound is written.
    #[test]
    fn cpu_max_is_compared_by_its_values_not_its_spelling() {
        let parse = CpuMax::parse;
        assert_eq!(
            parse("max 100000"),
            Some(CpuMax {
                quota_us: None,
                period_us: 100_000
            })
        );
        assert_eq!(
            parse("150000 100000\n"),
            Some(CpuMax {
                quota_us: Some(150_000),
                period_us: 100_000
            })
        );
        assert_eq!(
            parse(" max\t100000 \n"),
            parse("max 100000"),
            "spaced otherwise, the same"
        );
        assert_ne!(parse("max 100000"), parse("max 50000"), "another period");
        assert_ne!(
            parse("150000 100000"),
            parse("max 100000"),
            "a quota against none"
        );
        for bad in [
            "",
            "max",
            "100000",
            "max 100000 7",
            "x 100000",
            "max y",
            "-1 100000",
            "max -1",
        ] {
            assert_eq!(parse(bad), None, "{bad:?} isn't cpu.max's two values");
        }
        let ws = writes_all(&Bounds::default(), &[]).unwrap();
        let cpu = ws.iter().find(|w| w.file == "cpu.max").unwrap();
        assert_eq!(parse(&cpu.value), parse("max 100000"), "the default parses");
    }

    proptest! {
        // Hostile input (CLAUDE.md §6): whatever the files hold, reading them returns a value.
        #[test]
        fn usage_of_any_bytes_is_a_value_not_a_panic(a in ".*", b in ".*", c in ".*", d in "\\PC*") {
            let f = UsageFiles { memory_current: a.clone(), memory_peak: b, memory_events: c.clone(), pids_current: a, pids_events: c.clone(), cpu_stat: c, io_stat: d.clone() };
            let _ = usage(&f);
            let _ = populated(&d);
        }

        #[test]
        fn cpu_max_of_any_text_is_a_value_not_a_panic(t in "\\PC*") {
            let _ = CpuMax::parse(&t);
        }

        #[test]
        fn cpu_max_holds_the_asked_share_of_each_period(milli in 10u32..=u32::MAX) {
            let ws = writes(&Bounds { cpu_max_milli: Some(milli), ..Bounds::default() }, &[]).unwrap();
            let v = &ws[0].value;
            let (quota, period) = v.split_once(' ').unwrap();
            prop_assert_eq!(period, "100000");
            prop_assert_eq!(quota.parse::<u64>().unwrap(), u64::from(milli) * 100);
        }
    }
}
