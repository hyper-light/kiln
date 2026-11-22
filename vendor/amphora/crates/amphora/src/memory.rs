//! The memory this process may use, as the system says, and the budget that shares it out
//! (docs/design/store.md, decisions 26 and "Decision (lane constants): one process memory budget";
//! MB.1). The limit is the smallest of: the memory limits of the control groups the process runs in
//! (cgroup v2, else v1) and the machine's own memory on Linux; the machine's physical memory on
//! macOS (`hw.memsize`) and Windows (`GlobalMemoryStatusEx`); and, on Unix, the process's own soft
//! address-space, data and resident-set limits (`getrlimit`). Where none of them can be read the
//! budget refuses, typed, naming the platform: no fallback is picked.
//!
//! The parsers are pure, so tests give them each file's text; [`limit`] reads the files.

use amphora_api::Refusal;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, OnceLock};

/// A cgroup v1 limit at or past this is "no limit" (the kernel reports its page-rounded
/// `LONG_MAX`, 9,223,372,036,854,771,712 on 4 KiB pages).
const V1_UNLIMITED: u64 = 1 << 62;

/// The path of this process's cgroup v2 group, from `/proc/self/cgroup`'s `0::` line.
#[must_use]
pub fn v2_group(proc_self_cgroup: &str) -> Option<&str> {
    proc_self_cgroup
        .lines()
        .find_map(|line| line.strip_prefix("0::"))
        .map(str::trim)
}

/// The path of this process's cgroup v1 memory group, from `/proc/self/cgroup`'s line whose
/// controllers include `memory`.
#[must_use]
pub fn v1_group(proc_self_cgroup: &str) -> Option<&str> {
    proc_self_cgroup.lines().find_map(|line| {
        let mut fields = line.splitn(3, ':');
        let (_, controllers, path) = (fields.next()?, fields.next()?, fields.next()?);
        controllers
            .split(',')
            .any(|c| c == "memory")
            .then(|| path.trim())
    })
}

/// A cgroup's memory limit in bytes from its `memory.max` (v2) or `memory.limit_in_bytes` (v1):
/// None for "max" and for v1's no-limit value.
#[must_use]
pub fn group_limit(text: &str) -> Option<u64> {
    let text = text.trim();
    if text == "max" {
        return None;
    }
    text.parse::<u64>().ok().filter(|&n| n < V1_UNLIMITED)
}

/// The machine's memory in bytes, from `/proc/meminfo`'s `MemTotal:` line (in KiB).
#[must_use]
pub fn mem_total(meminfo: &str) -> Option<u64> {
    let line = meminfo
        .lines()
        .find_map(|line| line.strip_prefix("MemTotal:"))?;
    let kib = line.split_whitespace().next()?.parse::<u64>().ok()?;
    kib.checked_mul(1024)
}

/// Each group from `path` up to the hierarchy's root, deepest first: `/a/b` gives `/a/b`, `/a` and
/// `/` (a limit anywhere above a group bounds it too).
#[must_use]
pub fn ancestors(path: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut at = path.trim_end_matches('/');
    while !at.is_empty() {
        out.push(at.to_owned());
        at = match at.rfind('/') {
            Some(0) | None => "",
            Some(cut) => at.get(..cut).unwrap_or_default(),
        };
    }
    out.push("/".to_owned());
    out
}

/// The memory this process may use, in bytes: the smallest of what the platform says (the module's
/// head lists the readings). None where nothing can be read.
#[must_use]
pub fn limit() -> Option<u64> {
    let mut limits = platform_limits();
    limits.extend(process_limits());
    limits.into_iter().min()
}

/// Linux: the limits of every control group above this process (v2 and v1) and the machine's
/// memory (`/proc/meminfo`).
#[cfg(target_os = "linux")]
fn platform_limits() -> Vec<u64> {
    let read = |path: &str| std::fs::read_to_string(path).ok();
    let cgroup = read("/proc/self/cgroup").unwrap_or_default();
    let mut limits: Vec<u64> = Vec::new();
    if let Some(group) = v2_group(&cgroup) {
        for at in ancestors(group) {
            let dir = format!("/sys/fs/cgroup{}", if at == "/" { "" } else { at.as_str() });
            if let Some(n) = read(&format!("{dir}/memory.max"))
                .as_deref()
                .and_then(group_limit)
            {
                limits.push(n);
            }
        }
    }
    if let Some(group) = v1_group(&cgroup) {
        for at in ancestors(group) {
            let dir = format!(
                "/sys/fs/cgroup/memory{}",
                if at == "/" { "" } else { at.as_str() }
            );
            if let Some(n) = read(&format!("{dir}/memory.limit_in_bytes"))
                .as_deref()
                .and_then(group_limit)
            {
                limits.push(n);
            }
        }
    }
    if let Some(n) = read("/proc/meminfo").as_deref().and_then(mem_total) {
        limits.push(n);
    }
    limits
}

/// macOS and Windows: the machine's physical memory as the platform reports it (the engine makes
/// the call: this crate denies `unsafe`). A Windows job object's memory limit isn't read (it needs
/// `QueryInformationJobObject`): the machine's memory bounds such a process from above.
#[cfg(not(target_os = "linux"))]
fn platform_limits() -> Vec<u64> {
    amphora_engine::io::system_memory::physical_memory()
        .into_iter()
        .collect()
}

/// The process's own soft limits on what it may map (POSIX `getrlimit`: `RLIMIT_AS`,
/// `RLIMIT_DATA`, and `RLIMIT_RSS` where the system enforces it), each where one is set.
#[cfg(unix)]
fn process_limits() -> Vec<u64> {
    use rustix::process::{Resource, getrlimit};
    [Resource::As, Resource::Data, Resource::Rss]
        .into_iter()
        .filter_map(|resource| getrlimit(resource).current)
        .collect()
}

/// Windows has no per-process soft limit of this kind (its job objects aside, above).
#[cfg(windows)]
fn process_limits() -> Vec<u64> {
    Vec::new()
}

/// The bytes this process holds resident now (Linux `/proc/self/statm`, macOS `task_info`,
/// Windows `GetProcessMemoryInfo`, through memory-stats). None where it can't be read.
#[must_use]
pub fn resident() -> Option<u64> {
    memory_stats::memory_stats().and_then(|stats| u64::try_from(stats.physical_mem).ok())
}

/// What the process's memory is shared out to, each by its own measured use (MB.1): the owners'
/// queues (commands waiting, messages going out), the images a replica makes and installs, and
/// the caches (the page pool and what else keeps data to read again).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Use {
    Queues,
    Images,
    Caches,
}

impl Use {
    const ALL: [Self; 3] = [Self::Queues, Self::Images, Self::Caches];

    fn name(self) -> &'static str {
        match self {
            Self::Queues => "the owners' queues",
            Self::Images => "an image",
            Self::Caches => "the caches",
        }
    }
}

/// One reading of what the budget is learned from: the limit and the process's resident bytes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Reading {
    pub limit: Option<u64>,
    pub resident: Option<u64>,
}

/// Where a budget reads from: the system's own readings, or a test's fed ones.
pub trait Readings: Send + Sync {
    fn read(&self) -> Reading;
}

/// The system's readings, taken again at every ask: a host may change a limit while the process runs.
struct System;

impl Readings for System {
    fn read(&self) -> Reading {
        Reading {
            limit: limit(),
            resident: resident(),
        }
    }
}

/// The process's memory budget (MB.1): its limit, read where it runs, shared out to each [`Use`]
/// as that use's measured bytes. Each use charges what it holds as it takes it ([`Budget::take`])
/// and gives it back as it lets it go; a use's room is the limit less what everything else holds,
/// the larger of the process's resident bytes and the charges counted, its own charge given back.
/// No share is picked: a use has what the others leave, as measured now.
pub struct Budget {
    readings: Box<dyn Readings>,
    held: [AtomicU64; 3],
}

/// Bytes held against a budget by one use, given back when this drops.
pub struct Charge {
    budget: Arc<Budget>,
    used: Use,
    bytes: u64,
}

impl Charge {
    /// The bytes this charge holds.
    #[must_use]
    pub fn bytes(&self) -> u64 {
        self.bytes
    }
}

impl Drop for Charge {
    fn drop(&mut self) {
        self.budget.give(self.used, self.bytes);
    }
}

/// The engine's versioned storage takes its memory from the process's budget as caches
/// ([`Use::Caches`]: its page frames and row versions), what the rest of the process leaves it by
/// measured use (G7 step 2, `AC-NS.2.1`; MB.1). Every database the process opens shares the one
/// budget.
pub struct StorageMemory(pub Arc<Budget>);

impl amphora_engine::storage::versioned::memory::Source for StorageMemory {
    fn take(&self, what: &'static str, bytes: u64) -> amphora_engine::Result<()> {
        self.0
            .hold(Use::Caches, bytes)
            .map_err(|refusal| match refusal {
                Refusal::TooLarge { limit, .. } => amphora_engine::LimboError::MemoryBudget {
                    what,
                    asked: bytes,
                    held: self.0.held(Use::Caches),
                    limit: u64::try_from(limit).unwrap_or(u64::MAX),
                },
                other => amphora_engine::LimboError::InternalError(format!(
                    "versioned storage: {what}: {other}"
                )),
            })
    }

    fn give(&self, bytes: u64) {
        self.0.give(Use::Caches, bytes);
    }
}

impl Budget {
    /// A budget over `readings`.
    #[must_use]
    pub fn new(readings: Box<dyn Readings>) -> Arc<Self> {
        Arc::new(Self {
            readings,
            held: Default::default(),
        })
    }

    /// The counter of what `used` holds.
    fn slot(&self, used: Use) -> &AtomicU64 {
        let [queues, images, caches] = &self.held;
        match used {
            Use::Queues => queues,
            Use::Images => images,
            Use::Caches => caches,
        }
    }

    /// The bytes `used` holds now, as charged.
    #[must_use]
    pub fn held(&self, used: Use) -> u64 {
        self.slot(used).load(Ordering::Acquire)
    }

    /// The bytes `used` may hold at most now: the limit less what the rest of the process holds
    /// (the larger of its resident bytes and every charge counted, `used`'s own given back).
    /// Refused, typed and naming the platform, when the limit can't be read.
    pub fn room(&self, used: Use) -> Result<u64, Refusal> {
        let reading = self.readings.read();
        let limit = reading.limit.ok_or_else(|| unread("memory limit"))?;
        let charged = Use::ALL
            .iter()
            .fold(0u64, |sum, each| sum.saturating_add(self.held(*each)));
        let resident = reading.resident.unwrap_or(charged);
        let others = resident.max(charged).saturating_sub(self.held(used));
        Ok(limit.saturating_sub(others))
    }

    /// `bytes` more held by `used`, refused typed (past its room, [`Refusal::TooLarge`] naming the
    /// use and the room) when they would pass it.
    pub fn take(self: &Arc<Self>, used: Use, bytes: u64) -> Result<Charge, Refusal> {
        self.hold(used, bytes)?;
        Ok(Charge {
            budget: Arc::clone(self),
            used,
            bytes,
        })
    }

    /// `bytes` more held by `used`, as [`Budget::take`], for a holder that counts its own and
    /// gives them back with [`Budget::give`] (the engine's versioned storage, which takes a run of
    /// frames or a step of its versions' reservation at a time).
    pub fn hold(&self, used: Use, bytes: u64) -> Result<(), Refusal> {
        let room = self.room(used)?;
        self.slot(used)
            .try_update(Ordering::AcqRel, Ordering::Acquire, |held| {
                held.checked_add(bytes).filter(|after| *after <= room)
            })
            .map(|_| ())
            .map_err(|held| Refusal::TooLarge {
                what: format!("memory for {}", used.name()),
                size: usize::try_from(held.saturating_add(bytes)).unwrap_or(usize::MAX),
                limit: usize::try_from(room).unwrap_or(usize::MAX),
            })
    }

    /// `bytes` held by `used` ([`Budget::hold`]) given back.
    pub fn give(&self, used: Use, bytes: u64) {
        let _ = self
            .slot(used)
            .try_update(Ordering::AcqRel, Ordering::Acquire, |held| {
                Some(held.saturating_sub(bytes))
            });
    }

    /// The room for one image: [`Use::Images`]' room, in the `usize` an image's bytes are
    /// counted in (the seam owner's ruling (c): an image is bounded by the memory budget's room
    /// for one; SB.1).
    pub fn image_room(&self) -> Result<usize, Refusal> {
        self.room(Use::Images)
            .map(|room| usize::try_from(room).unwrap_or(usize::MAX))
    }
}

/// The refusal where `what` can't be read on this platform.
fn unread(what: &str) -> Refusal {
    Refusal::Unavailable(format!(
        "this process's {what} can't be read on {} ({}): no memory budget",
        std::env::consts::OS,
        std::env::consts::ARCH
    ))
}

/// The process's budget, over the system's readings; a test's own, where it set one on its thread.
#[must_use]
pub fn process() -> Arc<Budget> {
    #[cfg(test)]
    if let Some(budget) = tests::BUDGET.with(|budget| budget.borrow().clone()) {
        return budget;
    }
    static PROCESS: OnceLock<Arc<Budget>> = OnceLock::new();
    PROCESS
        .get_or_init(|| Budget::new(Box::new(System)))
        .clone()
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use std::cell::RefCell;
    use std::sync::Mutex;

    thread_local! {
        /// A test's budget for what it opens on its thread, in place of the process's.
        pub(crate) static BUDGET: RefCell<Option<Arc<Budget>>> = const { RefCell::new(None) };
    }

    /// Readings a test feeds and changes as it goes.
    #[derive(Clone, Default)]
    pub(crate) struct Fed(pub(crate) Arc<Mutex<Option<Reading>>>);

    impl Fed {
        pub(crate) fn new(limit: Option<u64>, resident: Option<u64>) -> Self {
            Self(Arc::new(Mutex::new(Some(Reading { limit, resident }))))
        }

        pub(crate) fn set(&self, limit: Option<u64>, resident: Option<u64>) {
            if let Ok(mut reading) = self.0.lock() {
                *reading = Some(Reading { limit, resident });
            }
        }
    }

    impl Readings for Fed {
        fn read(&self) -> Reading {
            self.0.lock().ok().and_then(|r| *r).unwrap_or(Reading {
                limit: None,
                resident: None,
            })
        }
    }

    /// A budget over `fed`, set as this thread's process budget until the guard drops.
    pub(crate) fn budget(fed: &Fed) -> (Arc<Budget>, impl Drop) {
        struct Unset;
        impl Drop for Unset {
            fn drop(&mut self) {
                BUDGET.with(|budget| budget.borrow_mut().take());
            }
        }
        let budget = Budget::new(Box::new(fed.clone()));
        BUDGET.with(|slot| *slot.borrow_mut() = Some(Arc::clone(&budget)));
        (budget, Unset)
    }

    /// Do: a budget fed a 1 GiB limit and 100 MiB resident; take queue bytes, then image bytes;
    /// drop the queue's charge. Expect: each use's room is the limit less what the rest holds (the
    /// resident bytes until the charges pass them), a take past its room refused typed, and a
    /// charge given back on drop (MB.1, AC-MB.1.2).
    #[test]
    fn mb1_a_uses_room_is_what_the_rest_of_the_process_leaves() {
        let fed = Fed::new(Some(1 << 30), Some(100 << 20));
        let budget = Budget::new(Box::new(fed.clone()));
        assert_eq!(budget.room(Use::Images).unwrap(), (1 << 30) - (100 << 20));
        let queued = budget.take(Use::Queues, 600 << 20).unwrap();
        // The charges (600 MiB) now pass the resident bytes: the image has what they leave.
        assert_eq!(budget.room(Use::Images).unwrap(), (1 << 30) - (600 << 20));
        // The queue's own room gives its charge back.
        assert_eq!(budget.room(Use::Queues).unwrap(), 1 << 30);
        let refused = budget.take(Use::Images, 500 << 20);
        assert!(matches!(refused, Err(Refusal::TooLarge { what, limit, .. })
            if what.contains("image") && limit == (1 << 30) - (600 << 20)));
        let image = budget.take(Use::Images, 400 << 20).unwrap();
        assert_eq!(image.bytes(), 400 << 20);
        assert_eq!(budget.held(Use::Images), 400 << 20);
        drop(queued);
        assert_eq!(budget.held(Use::Queues), 0);
        // Resident bytes measured past the charges count as the rest's use.
        fed.set(Some(1 << 30), Some(900 << 20));
        assert_eq!(budget.room(Use::Images).unwrap(), (1 << 30) - (500 << 20));
        assert_eq!(budget.room(Use::Caches).unwrap(), (1 << 30) - (900 << 20));
        drop(image);
        assert_eq!(budget.held(Use::Images), 0);
    }

    /// Do: a budget whose limit can't be read, then is read, then is lowered as the process runs.
    /// Expect: a typed refusal naming the platform while unread, no fallback; then the room
    /// follows the limit as it moves (MB.1, AC-MB.1.1).
    #[test]
    fn mb1_no_limit_read_is_a_typed_refusal_and_the_limit_is_read_again() {
        let fed = Fed::new(None, Some(10 << 20));
        let budget = Budget::new(Box::new(fed.clone()));
        let refused = budget.room(Use::Queues);
        assert!(matches!(&refused, Err(Refusal::Unavailable(why))
            if why.contains(std::env::consts::OS) && why.contains("memory limit")));
        assert!(budget.take(Use::Queues, 1).is_err());
        fed.set(Some(64 << 20), Some(10 << 20));
        assert_eq!(budget.room(Use::Queues).unwrap(), 54 << 20);
        fed.set(Some(32 << 20), Some(10 << 20));
        assert_eq!(budget.room(Use::Queues).unwrap(), 22 << 20);
        // A limit below what is resident leaves no room, never a negative one.
        fed.set(Some(8 << 20), Some(10 << 20));
        assert_eq!(budget.room(Use::Images).unwrap(), 0);
        assert!(budget.take(Use::Images, 1).is_err());
        assert!(budget.take(Use::Images, 0).is_ok());
    }

    /// Do: a budget whose resident bytes can't be read. Expect: the charges counted stand for the
    /// rest's use (MB.1).
    #[test]
    fn mb1_unread_resident_bytes_fall_to_the_charges_counted() {
        let fed = Fed::new(Some(100), None);
        let budget = Budget::new(Box::new(fed));
        let held = budget.take(Use::Caches, 30).unwrap();
        assert_eq!(budget.room(Use::Images).unwrap(), 70);
        assert_eq!(budget.room(Use::Caches).unwrap(), 100);
        drop(held);
        assert_eq!(budget.room(Use::Images).unwrap(), 100);
    }

    /// Do: read this process's limit and resident bytes where it runs (Linux, macOS, Windows).
    /// Expect: both are read, the resident bytes within the limit (MB.1, AC-MB.1.1: learned where
    /// it runs, on all three).
    #[cfg(any(target_os = "linux", target_os = "macos", target_os = "windows"))]
    #[test]
    fn mb1_this_process_reads_its_limit_and_resident_bytes() {
        let limit = limit();
        let resident = resident();
        assert!(limit.is_some_and(|l| l > 0));
        assert!(resident.is_some_and(|r| r > 0));
        assert!(resident <= limit);
        assert!(process().room(Use::Images).is_ok());
    }

    /// Do: parse a cgroup v2 process's `/proc/self/cgroup`, a v1 process's, and a hybrid's. Expect:
    /// each memory group's path, none where there is none (AC-T1.13.4: reads at their latency).
    #[test]
    fn a_process_finds_its_memory_group_in_either_hierarchy() {
        let v2 = "0::/user.slice/user-1000.slice/app.slice/amphora.service\n";
        assert_eq!(
            v2_group(v2),
            Some("/user.slice/user-1000.slice/app.slice/amphora.service")
        );
        assert_eq!(v1_group(v2), None);
        let v1 = "12:pids:/a\n11:memory:/docker/abc\n10:cpu,cpuacct:/docker/abc\n";
        assert_eq!(v1_group(v1), Some("/docker/abc"));
        assert_eq!(v2_group(v1), None);
        let hybrid = "11:memory:/k8s/pod1\n0::/k8s/pod1\n";
        assert_eq!(v1_group(hybrid), Some("/k8s/pod1"));
        assert_eq!(v2_group(hybrid), Some("/k8s/pod1"));
        assert_eq!(v2_group("0::/\n"), Some("/"));
    }

    /// Do: read limits as the kernel writes them. Expect: bytes for a number, none for "max" and
    /// for v1's no-limit value, none for text that isn't one (AC-T1.13.4: reads at their latency).
    #[test]
    fn a_limit_is_a_number_of_bytes_or_none() {
        assert_eq!(group_limit("34359738368\n"), Some(34_359_738_368));
        assert_eq!(group_limit("max\n"), None);
        assert_eq!(group_limit("9223372036854771712\n"), None);
        assert_eq!(group_limit("garbage"), None);
        let meminfo = "MemTotal:       131072000 kB\nMemFree:        1000 kB\n";
        assert_eq!(mem_total(meminfo), Some(131_072_000 * 1024));
        assert_eq!(mem_total("MemFree: 1 kB\n"), None);
    }

    /// Do: list the groups above `/a/b/c`, `/a` and the root. Expect: deepest first, the root last,
    /// the root once (AC-T1.13.4: reads at their latency).
    #[test]
    fn a_groups_limit_is_bounded_by_every_group_above_it() {
        assert_eq!(ancestors("/a/b/c"), ["/a/b/c", "/a/b", "/a", "/"]);
        assert_eq!(ancestors("/a"), ["/a", "/"]);
        assert_eq!(ancestors("/"), ["/"]);
        assert_eq!(ancestors(""), ["/"]);
    }

    /// Do: ask this process's limit on Linux. Expect: some number of bytes, no more than the
    /// machine's memory (AC-T1.13.4: reads at their latency).
    #[cfg(target_os = "linux")]
    #[test]
    fn this_process_has_a_limit_no_larger_than_its_machine() {
        let machine = std::fs::read_to_string("/proc/meminfo")
            .ok()
            .as_deref()
            .and_then(mem_total);
        let got = limit();
        assert!(got.is_some());
        assert!(got <= machine);
    }
}
