use serde::{Deserialize, Serialize};

use crate::Refusal;

/// What a box may use. `None` leaves that resource to the box's parent: the box shares what its
/// parent has, as the kernel's defaults do. The names and units are cgroup v2's
/// (docs/kernel/admin-guide/cgroup-v2.rst), the `process` class's bound on Linux; other platforms
/// enforce what they can and refuse the rest by name.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Bounds {
    /// Share of CPU under contention (`cpu.weight`): 1 to 10,000, 100 being the default share.
    pub cpu_weight: Option<u16>,
    /// Most CPU it may use, in thousandths of a CPU (`cpu.max`): 1,500 is one and a half CPUs.
    pub cpu_max_milli: Option<u32>,
    /// Most memory, in bytes (`memory.max`): past it the kernel reclaims, then kills in the box. The
    /// box may not swap (`memory.swap.max` 0), so the bound holds on a host with swap too.
    pub memory_max: Option<u64>,
    /// Memory past which the box is throttled and reclaimed (`memory.high`), in bytes.
    pub memory_high: Option<u64>,
    /// Share of disk time under contention (`io.weight`): 1 to 10,000, 100 being the default.
    pub io_weight: Option<u16>,
    /// The box's disk latency target (`io.latency`), in microseconds: while the box misses it, its
    /// siblings with looser targets are throttled.
    pub io_latency_us: Option<u32>,
    /// Most disk throughput on each disk the node holds boxes' files on (`io.max`).
    pub io_max: Option<IoRates>,
    /// Most processes and threads (`pids.max`).
    pub pids_max: Option<u32>,
}

/// Throughput caps on one disk; each `None` is uncapped.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct IoRates {
    pub read_bps: Option<u64>,
    pub write_bps: Option<u64>,
    pub read_iops: Option<u64>,
    pub write_iops: Option<u64>,
}

/// Smallest `memory.max` Hex accepts: below a mebibyte no process starts, so a smaller bound is a
/// mistake to refuse rather than a box that dies at once.
pub const MIN_MEMORY: u64 = 1 << 20;
/// Smallest `cpu.max` in thousandths of a CPU: the kernel's least quota is 1 ms per 100 ms period.
pub const MIN_CPU_MILLI: u32 = 10;

impl Bounds {
    /// Whether every bound is one the kernel can hold; the first that isn't, by name.
    pub fn check(&self) -> Result<(), Refusal> {
        let invalid = |what: &'static str, why: String| Err(Refusal::Invalid { what, why });
        for (what, w) in [
            ("cpu_weight", self.cpu_weight),
            ("io_weight", self.io_weight),
        ] {
            if let Some(w) = w
                && !(1..=10_000).contains(&w)
            {
                return invalid(what, format!("{w} is outside 1..=10000"));
            }
        }
        if let Some(c) = self.cpu_max_milli
            && c < MIN_CPU_MILLI
        {
            return invalid("cpu_max_milli", format!("{c} is under {MIN_CPU_MILLI}"));
        }
        if let Some(m) = self.memory_max
            && m < MIN_MEMORY
        {
            return invalid("memory_max", format!("{m} bytes is under {MIN_MEMORY}"));
        }
        if let (Some(high), Some(max)) = (self.memory_high, self.memory_max)
            && high > max
        {
            return invalid("memory_high", format!("{high} is over memory_max {max}"));
        }
        if self.io_latency_us == Some(0) {
            return invalid("io_latency_us", "0 is no target".to_owned());
        }
        if let Some(r) = self.io_max {
            for (what, v) in [
                ("io_max.read_bps", r.read_bps),
                ("io_max.write_bps", r.write_bps),
                ("io_max.read_iops", r.read_iops),
                ("io_max.write_iops", r.write_iops),
            ] {
                if v == Some(0) {
                    return invalid(what, "0 would stop the box's disk use entirely".to_owned());
                }
            }
        }
        if self.pids_max == Some(0) {
            return invalid(
                "pids_max",
                "0 leaves no room for the box's own process".to_owned(),
            );
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn each_bound_out_of_the_kernels_range_is_refused_by_name() {
        let cases: Vec<(Bounds, &str)> = vec![
            (
                Bounds {
                    cpu_weight: Some(0),
                    ..Bounds::default()
                },
                "cpu_weight",
            ),
            (
                Bounds {
                    cpu_weight: Some(10_001),
                    ..Bounds::default()
                },
                "cpu_weight",
            ),
            (
                Bounds {
                    io_weight: Some(0),
                    ..Bounds::default()
                },
                "io_weight",
            ),
            (
                Bounds {
                    cpu_max_milli: Some(9),
                    ..Bounds::default()
                },
                "cpu_max_milli",
            ),
            (
                Bounds {
                    memory_max: Some(4096),
                    ..Bounds::default()
                },
                "memory_max",
            ),
            (
                Bounds {
                    memory_max: Some(1 << 30),
                    memory_high: Some(2 << 30),
                    ..Bounds::default()
                },
                "memory_high",
            ),
            (
                Bounds {
                    io_latency_us: Some(0),
                    ..Bounds::default()
                },
                "io_latency_us",
            ),
            (
                Bounds {
                    io_max: Some(IoRates {
                        write_bps: Some(0),
                        ..IoRates::default()
                    }),
                    ..Bounds::default()
                },
                "io_max.write_bps",
            ),
            (
                Bounds {
                    pids_max: Some(0),
                    ..Bounds::default()
                },
                "pids_max",
            ),
        ];
        for (b, name) in cases {
            match b.check() {
                Err(Refusal::Invalid { what, .. }) => assert_eq!(what, name),
                other => panic!("{b:?}: {other:?}"),
            }
        }
    }

    #[test]
    fn the_kernels_own_extremes_are_accepted() {
        let b = Bounds {
            cpu_weight: Some(10_000),
            cpu_max_milli: Some(MIN_CPU_MILLI),
            memory_max: Some(MIN_MEMORY),
            memory_high: Some(MIN_MEMORY),
            io_weight: Some(1),
            io_latency_us: Some(1),
            io_max: Some(IoRates {
                read_bps: Some(1),
                write_bps: None,
                read_iops: None,
                write_iops: Some(1),
            }),
            pids_max: Some(1),
        };
        assert_eq!(b.check(), Ok(()));
        assert_eq!(Bounds::default().check(), Ok(()));
    }
}
