//! Which resource a slowdown can be laid on, and what a cause needs before kiln acts on it (LB05
//! step 2, docs/protect.md "Causes"). A service slowed on a resource kiln's work didn't use can't be
//! sped up by stopping kiln's work, so a slowdown is laid on kiln only with evidence it could be
//! kiln's: kiln's work used that resource over the same span, or, for I/O, still had pages of its
//! own to write back (Linux writes dirty pages later, on the writer's account where the file system
//! supports it: cgroup-v2.rst "Writeback"). What isn't known is taken as kiln's, the side that
//! protects the service.
//!
//! What the disks delivered while a target was slow says what they take only if every disk written
//! was busy throughout ([`at_capacity`]): a disk idle part of the span wasn't the bottleneck, so a
//! slow fsync then (the CPU short, a lock held) teaches nothing of the disks. Before LB05 headroom
//! took what was delivered during any alarm for the disks' capacity (research f9).

use serde::{Deserialize, Serialize};

use super::budget::TICK_MAX_US;

/// A resource a service waits on.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Resource {
    Cpu,
    Memory,
    Io,
}

impl Resource {
    pub const ALL: [Resource; 3] = [Resource::Cpu, Resource::Memory, Resource::Io];

    /// Its name, as PSI's files have it (`cpu.pressure`, `/proc/pressure/io`).
    pub fn name(self) -> &'static str {
        match self {
            Resource::Cpu => "cpu",
            Resource::Memory => "memory",
            Resource::Io => "io",
        }
    }

    /// The resource a kiln server's pool waits on (`kiln::pool_health::QUEUES`): the compute pool's
    /// threads the CPU, the store's, the files' and kiln.db's writer's the disks. Others (DNS) wait on
    /// what kiln's work beside it doesn't use.
    pub fn of_pool(pool: &str) -> Option<Resource> {
        match pool {
            "compute" => Some(Resource::Cpu),
            "store" | "fs" | "store_write" => Some(Resource::Io),
            _ => None,
        }
    }
}

/// What kiln's own work did over a span, from its cgroup's counters (deltas, but for what it holds
/// at the span's end).
#[derive(Clone, Copy, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Use {
    /// CPU time used (µs: `cpu.stat` usage_usec).
    pub cpu_us: f64,
    /// Bytes read and written on its account (`io.stat` rbytes + wbytes), writeback included where
    /// the file system attributes it; none where its cgroup has no io controller to count them (a
    /// user's manager isn't given one by default: systemd's user@.service delegates pids, memory and
    /// cpu).
    pub io_bytes: Option<f64>,
    /// Pages of its own still to be written back (`memory.stat` file_dirty + file_writeback), bytes.
    pub dirty_bytes: f64,
    /// Memory it holds (`memory.current`), bytes.
    pub mem_bytes: f64,
}

/// Whether a slowdown can be laid on kiln's work.
#[derive(Clone, Debug, PartialEq)]
pub enum Verdict {
    /// It used the resource meanwhile: it may be the cause, and stopping it can help.
    May(String),
    /// It didn't: stopping it can't speed the service up.
    Not(String),
    /// What it did isn't known: taken as `May`, the side that protects the service.
    Unknown(String),
}

impl Verdict {
    /// Whether to act on it as kiln's.
    pub fn acts(&self) -> bool {
        !matches!(self, Verdict::Not(_))
    }

    pub fn why(&self) -> &str {
        match self {
            Verdict::May(w) | Verdict::Not(w) | Verdict::Unknown(w) => w,
        }
    }
}

/// Whether a slowdown on `r` can be kiln's, from what kiln's work did over the same span (`used`;
/// none: not known).
pub fn could_be_kiln(r: Resource, used: Option<Use>) -> Verdict {
    let Some(u) = used else {
        return Verdict::Unknown(format!("what kiln's work did with {} isn't known, so it may be the cause", r.name()));
    };
    let mb = |b: f64| format!("{:.0} MB", b / 1e6);
    match r {
        Resource::Cpu if u.cpu_us > 0.0 => Verdict::May(format!("kiln's work ran {:.2} s on the CPUs meanwhile", u.cpu_us / 1e6)),
        Resource::Cpu => Verdict::Not("kiln's work used no CPU meanwhile".to_owned()),
        Resource::Io => match u.io_bytes {
            Some(b) if b > 0.0 || u.dirty_bytes > 0.0 => Verdict::May(format!("kiln's work moved {} on the disks meanwhile, {} of its own still to write back", mb(b), mb(u.dirty_bytes))),
            None if u.dirty_bytes > 0.0 => Verdict::May(format!("kiln's work has {} of its own still to write back", mb(u.dirty_bytes))),
            None => Verdict::Unknown("kiln's work's I/O isn't counted (its cgroup has no io controller), so it may be the cause".to_owned()),
            Some(_) => Verdict::Not("kiln's work did no I/O meanwhile and has nothing of its own to write back".to_owned()),
        },
        Resource::Memory if u.mem_bytes > 0.0 => Verdict::May(format!("kiln's work holds {} of memory", mb(u.mem_bytes))),
        Resource::Memory => Verdict::Not("kiln's work holds no memory".to_owned()),
    }
}

/// How long a disk's busy counter can say it sat idle when it didn't (ms): two ticks, one at each end
/// of the span, at Linux's coarsest tick (`super::budget::TICK_MAX_US`), since the counter counts
/// ticks (iostats.rst, v6.12: "Since 5.0 this field counts jiffies when at least one request was
/// started or completed").
pub const IDLE_RESOLUTION_MS: f64 = 2.0 * TICK_MAX_US / 1e3;

/// Whether what the disks delivered over a span says what they take: every disk written over it
/// busy all of it, to the counter's resolution. `idle_ms`: the longest any disk written sat idle
/// (none: not known). The counter can count less busy time than there was (iostats.rst: "some I/O
/// time might be not accounted in case of concurrent requests"), never more, so a busy disk may be
/// taken for one with idle time, and its capacity go unlearned, the safe way: no capacity learned
/// caps nothing. Busy throughout doesn't prove a disk that serves requests in parallel full, either;
/// it is what fullness needs.
pub fn at_capacity(idle_ms: Option<f64>) -> Option<bool> {
    idle_ms.filter(|i| i.is_finite()).map(|i| i <= IDLE_RESOLUTION_MS)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// LB05 step 2: a slowdown on a resource kiln's work didn't touch is no cause to stop it, and
    /// what isn't known protects the service. Writeback still owed is I/O kiln's work causes after
    /// its writes: ignoring it would clear a gate whose dirty pages are what a sync waits behind.
    #[test]
    fn lb05_a_slowdown_is_laid_on_kiln_only_where_its_work_used_the_resource() {
        let idle = Use { io_bytes: Some(0.0), ..Use::default() };
        for r in Resource::ALL {
            assert!(!could_be_kiln(r, Some(idle)).acts(), "{r:?} laid on work that used nothing");
            let unknown = could_be_kiln(r, None);
            assert!(matches!(unknown, Verdict::Unknown(_)) && unknown.acts(), "{r:?}: unknown must protect the service");
        }
        let compiling = Use { cpu_us: 9e6, io_bytes: Some(0.0), ..Use::default() };
        assert!(could_be_kiln(Resource::Cpu, Some(compiling)).acts());
        assert!(!could_be_kiln(Resource::Io, Some(compiling)).acts(), "CPU only: no I/O cause");
        let flushing = Use { dirty_bytes: 6e9, io_bytes: Some(0.0), ..Use::default() };
        assert!(could_be_kiln(Resource::Io, Some(flushing)).acts(), "its own dirty pages still draining");
        assert!(!could_be_kiln(Resource::Cpu, Some(flushing)).acts(), "disk only: no CPU cause");
        let holding = Use { mem_bytes: 3e10, ..Use::default() };
        assert!(could_be_kiln(Resource::Memory, Some(holding)).acts());
        assert!(could_be_kiln(Resource::Io, Some(Use { io_bytes: Some(1.0), ..Use::default() })).why().contains("moved"));
        // I/O not counted: with nothing of its own to write back, not known, so the service's side.
        let uncounted = could_be_kiln(Resource::Io, Some(Use { cpu_us: 1.0, ..Use::default() }));
        assert!(matches!(uncounted, Verdict::Unknown(_)), "{uncounted:?}");
        assert!(could_be_kiln(Resource::Io, Some(Use { dirty_bytes: 1e6, ..Use::default() })).acts());
    }

    /// A disk idle part of the span isn't at capacity: 25 ms idle of a span is past what the counter
    /// can miss, 15 ms isn't.
    #[test]
    fn lb05_delivered_writes_are_capacity_only_with_every_disk_busy_throughout() {
        assert_eq!(IDLE_RESOLUTION_MS, 20.0);
        assert_eq!(at_capacity(Some(0.0)), Some(true));
        assert_eq!(at_capacity(Some(15.0)), Some(true));
        assert_eq!(at_capacity(Some(25.0)), Some(false));
        assert_eq!(at_capacity(Some(500.0)), Some(false));
        assert_eq!(at_capacity(None), None);
        assert_eq!(at_capacity(Some(f64::NAN)), None);
    }

    #[test]
    fn a_servers_pools_wait_on_what_they_wait_on() {
        assert_eq!(Resource::of_pool("compute"), Some(Resource::Cpu));
        for p in ["store", "fs", "store_write"] {
            assert_eq!(Resource::of_pool(p), Some(Resource::Io), "{p}");
        }
        assert_eq!(Resource::of_pool("dns"), None);
    }
}
