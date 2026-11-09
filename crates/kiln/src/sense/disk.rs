//! The work volume's device: its counters, read on demand by [`super::Sensor::measure`], and what
//! two reads of them say of the interval between (throughput, latency, queue, errors). Passive:
//! the OS's own counters of the I/O the machine does anyway, no test I/O (docs/needs.md;
//! `~/.cache/kiln-lb/notes/research-disk.md`).
//!
//! - Linux: `/sys/block/<disk>/stat` (the device's line of `/proc/diskstats`, world-readable):
//!   ops, sectors (512 bytes each, whatever the device's), milliseconds reading and writing,
//!   milliseconds busy, weighted milliseconds in flight.
//! - macOS: the `IOBlockStorageDriver`'s `Statistics`: bytes, operations, total time (ns) per
//!   direction, errors and retries. No busy counter.
//! - Windows: `IOCTL_DISK_PERFORMANCE` on a handle opened with no access (a restricted token may):
//!   bytes, counts, read and write time, idle time, in a unit its documentation doesn't give,
//!   calibrated against the monotonic clock by its own timestamp (`QueryTime`).
//!
//! Busy time says nothing of capacity on a device that serves requests in parallel (an NVMe
//! drive's 64K queues): U = XS fails there. So the share busy is said where the OS keeps it, and
//! latency and the mean number in flight carry the load.

use serde::{Deserialize, Serialize};

/// A device's error counters as its OS keeps them, cumulative. What the OS doesn't keep, or keeps
/// only for a privileged reader, is none.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Errors {
    /// Commands that ended in error: Linux SCSI's `ioerr_cnt` (any non-zero result), NVMe's
    /// `command_error_count` (Linux 7.2+), md's member `errors`, btrfs's read, write, flush,
    /// corruption and generation errors, ext4's `errors_count`; macOS's read and write errors.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub errors: Option<u64>,
    /// Commands that timed out (Linux SCSI's `iotmo_cnt`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub timeouts: Option<u64>,
    /// Commands retried (NVMe's `command_retries_count`; macOS's read and write retries).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub retries: Option<u64>,
    /// Controller resets and reconnects (NVMe's `reset_count` and `reconnect_count`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub resets: Option<u64>,
}

fn sum(a: Option<u64>, b: Option<u64>) -> Option<u64> {
    match (a, b) {
        (Some(a), Some(b)) => Some(a.saturating_add(b)),
        (a, b) => a.or(b),
    }
}

impl Errors {
    /// Both sets of counters added (a file system over several devices).
    pub fn plus(self, o: Errors) -> Errors {
        Errors { errors: sum(self.errors, o.errors), timeouts: sum(self.timeouts, o.timeouts), retries: sum(self.retries, o.retries), resets: sum(self.resets, o.resets) }
    }

    /// What each counter rose by from `earlier`; none where either read lacks it or it went back
    /// (a reset of the counters, a device replaced).
    pub fn since(self, earlier: Errors) -> Errors {
        let d = |l: Option<u64>, e: Option<u64>| l?.checked_sub(e?);
        Errors { errors: d(self.errors, earlier.errors), timeouts: d(self.timeouts, earlier.timeouts), retries: d(self.retries, earlier.retries), resets: d(self.resets, earlier.resets) }
    }

    /// Any counter at all.
    pub fn any(&self) -> bool {
        self.errors.is_some() || self.timeouts.is_some() || self.retries.is_some() || self.resets.is_some()
    }
}

/// One direction's cumulative counters.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Dir {
    pub ops: u64,
    pub bytes: u64,
    /// Time the direction's requests took, summed over requests (queueing included), in ns.
    pub time_ns: u64,
}

/// A device's cumulative counters, as read now.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Counters {
    pub read: Dir,
    pub write: Dir,
    /// Time with at least one request in flight, in ns (Linux's `io_ticks`, Windows' all time
    /// less idle); none on macOS.
    pub busy_ns: Option<u64>,
    /// Requests in flight integrated over time, in request-ns (Linux's weighted time); none where
    /// the OS keeps none, and the mean in flight is then the directions' time over the interval.
    pub in_flight_ns: Option<u64>,
    pub errors: Errors,
}

/// What the device did over an interval: from two reads of its [`Counters`].
#[derive(Debug, Clone, Copy, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Activity {
    pub read_bytes_per_s: f64,
    pub write_bytes_per_s: f64,
    pub read_ops_per_s: f64,
    pub write_ops_per_s: f64,
    /// Mean time a read took, queueing included, in µs; none without reads in the interval.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub read_latency_us: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub write_latency_us: Option<f64>,
    /// Mean requests in flight (Little's law: their time over the interval).
    pub in_flight: f64,
    /// The share of the interval with a request in flight, where the OS keeps it. A device that
    /// serves requests in parallel can be busy all the time and far from full.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub busy: Option<f64>,
    /// Errors over the interval.
    pub errors: Errors,
    /// The interval, in µs.
    pub interval_us: u64,
}

impl Activity {
    /// The interval from `earlier` to `later`, `interval_us` long; none for an empty interval or
    /// counters that went back (a reset, another device).
    pub fn between(earlier: &Counters, later: &Counters, interval_us: u64) -> Option<Activity> {
        if interval_us == 0 {
            return None;
        }
        let d = |l: u64, e: u64| l.checked_sub(e);
        let (rops, wops) = (d(later.read.ops, earlier.read.ops)?, d(later.write.ops, earlier.write.ops)?);
        let (rb, wb) = (d(later.read.bytes, earlier.read.bytes)?, d(later.write.bytes, earlier.write.bytes)?);
        let (rt, wt) = (d(later.read.time_ns, earlier.read.time_ns)?, d(later.write.time_ns, earlier.write.time_ns)?);
        let secs = interval_us as f64 / 1e6;
        let span_ns = interval_us as f64 * 1e3;
        let latency = |t: u64, n: u64| (n > 0).then(|| t as f64 / n as f64 / 1e3);
        let in_flight = match (earlier.in_flight_ns, later.in_flight_ns) {
            (Some(e), Some(l)) if l >= e => (l - e) as f64 / span_ns,
            _ => (rt + wt) as f64 / span_ns,
        };
        let busy = match (earlier.busy_ns, later.busy_ns) {
            (Some(e), Some(l)) if l >= e => Some(((l - e) as f64 / span_ns).clamp(0.0, 1.0)),
            _ => None,
        };
        Some(Activity {
            read_bytes_per_s: rb as f64 / secs,
            write_bytes_per_s: wb as f64 / secs,
            read_ops_per_s: rops as f64 / secs,
            write_ops_per_s: wops as f64 / secs,
            read_latency_us: latency(rt, rops),
            write_latency_us: latency(wt, wops),
            in_flight,
            busy,
            errors: later.errors.since(earlier.errors),
            interval_us,
        })
    }
}

/// Parsing of Linux's block files, pure so it's tested everywhere.
pub mod linux {
    use super::{Counters, Dir};

    /// A block device's `stat` (the fields of its `/proc/diskstats` line after its name): reads,
    /// reads merged, sectors read, ms reading, writes, merged, sectors written, ms writing, in
    /// flight, ms busy, weighted ms. Sectors are 512 bytes whatever the device's own size.
    pub fn stat(text: &str) -> Option<Counters> {
        let v: Vec<u64> = text.split_whitespace().take(11).map(|w| w.parse::<u64>()).collect::<Result<_, _>>().ok()?;
        let f = |i: usize| v.get(i).copied();
        let ms = |i: usize| f(i).map(|m| m.saturating_mul(1_000_000));
        Some(Counters {
            read: Dir { ops: f(0)?, bytes: f(2)?.saturating_mul(512), time_ns: ms(3)? },
            write: Dir { ops: f(4)?, bytes: f(6)?.saturating_mul(512), time_ns: ms(7)? },
            busy_ns: ms(9),
            in_flight_ns: ms(10),
            errors: super::Errors::default(),
        })
    }

    /// A `/proc/diskstats` line's device's counters, for the device named `name`.
    pub fn diskstats(text: &str, name: &str) -> Option<Counters> {
        text.lines().find_map(|l| {
            let mut w = l.split_whitespace();
            let (_, _, n) = (w.next()?, w.next()?, w.next()?);
            (n == name).then(|| stat(&w.collect::<Vec<_>>().join(" "))).flatten()
        })
    }

    /// A sysfs counter as the kernel writes it: decimal, or hex with `0x` (SCSI's `ioerr_cnt`).
    pub fn counter(text: &str) -> Option<u64> {
        let t = text.trim();
        match t.strip_prefix("0x") {
            Some(h) => u64::from_str_radix(h, 16).ok(),
            None => t.parse().ok(),
        }
    }

    /// btrfs's per-device `error_stats` (`write_errs 0`, `read_errs 0`, `flush_errs 0`,
    /// `corruption_errs 0`, `generation_errs 0`): their sum.
    pub fn btrfs_errors(text: &str) -> Option<u64> {
        let mut any = false;
        let mut n = 0u64;
        for l in text.lines() {
            if let Some((k, v)) = l.split_once(' ')
                && k.ends_with("_errs")
                && let Ok(v) = v.trim().parse::<u64>()
            {
                any = true;
                n = n.saturating_add(v);
            }
        }
        any.then_some(n)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// docs/needs.md: passive throughput and latency are deltas of the OS's counters: bytes over
    /// the interval, the direction's time over its ops, time in flight over the interval.
    #[test]
    fn w13_r01_device_activity_from_two_reads() {
        // nvme0n1 a second apart: 100 reads of 8 sectors in 50 ms, 10 writes of 2048 sectors in
        // 30 ms, busy 70 ms, 90 ms in flight weighted.
        let a = linux::stat("1000 0 8000 500 100 0 204800 300 0 700 900 0 0 0 0 0 0").unwrap_or_default();
        let b = linux::stat("1100 0 8800 550 110 0 225280 330 2 770 990 0 0 0 0 0 0").unwrap_or_default();
        let act = Activity::between(&a, &b, 1_000_000);
        let want = Activity {
            read_bytes_per_s: 800.0 * 512.0,
            write_bytes_per_s: 20480.0 * 512.0,
            read_ops_per_s: 100.0,
            write_ops_per_s: 10.0,
            read_latency_us: Some(500.0),
            write_latency_us: Some(3000.0),
            in_flight: 0.09,
            busy: Some(0.07),
            errors: Errors::default(),
            interval_us: 1_000_000,
        };
        assert_eq!(act.map(|mut a| {
            a.in_flight = (a.in_flight * 1e9).round() / 1e9;
            a.busy = a.busy.map(|b| (b * 1e9).round() / 1e9);
            a
        }), Some(want));
        assert_eq!(Activity::between(&b, &a, 1_000_000), None, "counters that went back");
        assert_eq!(Activity::between(&a, &b, 0), None);
        // No reads in the interval: no read latency, rather than zero.
        assert_eq!(Activity::between(&a, &a, 1).and_then(|a| a.read_latency_us), None);
    }

    #[test]
    fn w13_r01_device_errors_rise_or_say_nothing() {
        let a = Errors { errors: Some(3), timeouts: Some(0), retries: None, resets: Some(1) };
        let b = Errors { errors: Some(5), timeouts: Some(0), retries: Some(9), resets: Some(0) };
        assert_eq!(b.since(a), Errors { errors: Some(2), timeouts: Some(0), retries: None, resets: None });
        assert_eq!(a.plus(b), Errors { errors: Some(8), timeouts: Some(0), retries: Some(9), resets: Some(1) });
        assert_eq!(linux::counter("0x1a\n"), Some(26));
        assert_eq!(linux::counter("7\n"), Some(7));
        assert_eq!(linux::btrfs_errors("write_errs 0\nread_errs 2\nflush_errs 0\ncorruption_errs 1\ngeneration_errs 0\n"), Some(3));
        assert_eq!(linux::btrfs_errors(""), None);
        let ds = "   8       0 sda 10 0 80 5 2 0 16 1 0 6 6 0 0 0 0 0 0\n 259       0 nvme0n1 1 0 8 1 0 0 0 0 0 1 1 0 0 0 0 0 0\n";
        assert_eq!(linux::diskstats(ds, "nvme0n1").map(|c| c.read.bytes), Some(4096));
        assert_eq!(linux::diskstats(ds, "sdb"), None);
    }
}
