//! What the server's memory holds, by owner (goal 11 of the API runtime program, plan 01a1a87b2e56 steps 1
//! and 3), on `/metrics`:
//!
//! - `kiln_memory_resident_bytes{kind}`: what the OS holds of the process, by kind (Linux,
//!   `/proc/self/status`): `anon` (the heap, threads' stacks, what the allocator keeps that it hasn't given
//!   back), `file` (the binary's and its libraries' pages read in, which the OS can take back and read again)
//!   and `shmem`. `process_resident_memory_bytes` is their sum.
//! - `kiln_memory_bytes{owner}`: what each of the server's caches and buffers holds now: counted as it
//!   changes (an atomic each: no lock, and a scrape reads them without asking the owner anything), or read at
//!   the scrape where the owner keeps its own count.
//! - `kiln_memory_bound_bytes{owner}`: the most an owner may hold, and why is said where the bound is set.

use std::fmt::Write as _;
use std::sync::atomic::{AtomicUsize, Ordering};

pub mod owners;
pub mod queue;
pub mod ring;

/// One owner of memory: what it holds now and the most it may hold (0: not known yet).
pub struct Owner {
    name: &'static str,
    bytes: AtomicUsize,
    bound: AtomicUsize,
}

impl Owner {
    const fn new(name: &'static str) -> Self {
        Self { name, bytes: AtomicUsize::new(0), bound: AtomicUsize::new(0) }
    }

    /// Its name on `/metrics` (`owner="…"`).
    pub fn name(&self) -> &'static str {
        self.name
    }

    /// It took `n` bytes more.
    pub fn add(&self, n: usize) {
        self.bytes.fetch_add(n, Ordering::Relaxed);
    }

    /// It let `n` bytes go: never below none, so a count that drifted says none rather than a wrapped one.
    pub fn sub(&self, n: usize) {
        let mut now = self.bytes.load(Ordering::Relaxed);
        while let Err(seen) = self.bytes.compare_exchange_weak(now, now.saturating_sub(n), Ordering::Relaxed, Ordering::Relaxed) {
            now = seen;
        }
    }

    /// What it holds now.
    pub fn bytes(&self) -> usize {
        self.bytes.load(Ordering::Relaxed)
    }

    /// The most it may hold, said with its basis where it's set.
    pub fn bound_at(&self, n: usize) {
        self.bound.store(n, Ordering::Relaxed);
    }

    /// The most it may hold, once known.
    pub fn bound(&self) -> Option<usize> {
        Some(self.bound.load(Ordering::Relaxed)).filter(|b| *b > 0)
    }

    /// A queue of its was made, bounded at `n` bytes ([`queue::Room`]): an owner of many queues (one a
    /// shard, one a viewer) is bounded by their sum.
    pub fn bound_add(&self, n: usize) {
        self.bound.fetch_add(n, Ordering::Relaxed);
    }

    /// A queue of its went, and its bound with it: never below none.
    pub fn bound_sub(&self, n: usize) {
        let mut now = self.bound.load(Ordering::Relaxed);
        while let Err(seen) = self.bound.compare_exchange_weak(now, now.saturating_sub(n), Ordering::Relaxed, Ordering::Relaxed) {
            now = seen;
        }
    }
}

/// The UI's files kept compressed, brotli and gzip, for the requests after the first (`crate::ui`): this
/// build's own files alone, so its bound is what they take compressed both ways, known once the server has
/// made them as it starts.
pub static UI_ASSETS: Owner = Owner::new("ui_assets");

/// Session copies' events and changeset files as their JSON's text, where it's kept as text (not packed):
/// each copy's newest events, and what a reader asked for of a packed chunk while it holds it
/// (`crate::harness::registry::Entry`). Its bound is each copy's (`crate::harness::core::MAX_EVENTS`
/// events, its newest chunk alone kept as text), for as many copies as there are sessions.
pub static SESSION_ENTRIES: Owner = Owner::new("session_entries");

/// The rows the conversation store's writer hasn't committed yet (`crate::convo::store::Queue`): its bound
/// is what the writer commits in the most a row waits to share a commit (`COMMIT_MS`), at the rate it has
/// committed.
pub static CONVO_QUEUE: Owner = Owner::new("convo_queue");

/// Viewers' snapshots made and not yet handed to their sockets (`crate::api`'s `send_snapshot`): a long
/// session's is megabytes, packed or as frames, and each viewer that opens one holds its own until the last
/// of it is written. No bound of its own yet: as many as viewers open sessions at once.
pub static SNAPSHOTS: Owner = Owner::new("snapshots");

/// The owners counted here, in the order the exposition says them.
const OWNERS: [&Owner; 4] = [&UI_ASSETS, &SESSION_ENTRIES, &CONVO_QUEUE, &SNAPSHOTS];

/// Bytes an owner holds for as long as this lives: what's left of them is let go with it, whether they
/// were written or dropped on the way (an error, a socket gone).
pub struct Held {
    owner: &'static Owner,
    bytes: usize,
}

impl Owner {
    /// `bytes` held until what's returned lets them go.
    pub fn hold(&'static self, bytes: usize) -> Held {
        self.add(bytes);
        Held { owner: self, bytes }
    }
}

impl Held {
    /// `n` of its bytes gone now (a frame written), never more than it holds.
    pub fn release(&mut self, n: usize) {
        let n = n.min(self.bytes);
        self.owner.sub(n);
        self.bytes = self.bytes.saturating_sub(n);
    }
}

impl Drop for Held {
    fn drop(&mut self) {
        self.owner.sub(self.bytes);
    }
}

/// The process's resident memory by kind, in bytes: `anon`, `file` and `shmem` (Linux; elsewhere none).
pub fn resident() -> Option<[(&'static str, u64); 3]> {
    let status = std::fs::read_to_string("/proc/self/status").ok()?;
    resident_of(&status)
}

/// [`resident`] from the text of `/proc/<pid>/status`: its `RssAnon`, `RssFile` and `RssShmem`, in kB.
fn resident_of(status: &str) -> Option<[(&'static str, u64); 3]> {
    let kb = |key: &str| -> Option<u64> { status.lines().find_map(|l| l.strip_prefix(key))?.split_whitespace().next()?.parse::<u64>().ok()?.checked_mul(1024) };
    Some([("anon", kb("RssAnon:")?), ("file", kb("RssFile:")?), ("shmem", kb("RssShmem:")?)])
}

/// The `kiln_memory_*` families, in the exposition: the owners counted here, and those that keep their own
/// count (`others`, each its name, what it holds and its bound).
pub fn render(o: &mut String, others: &[(&str, u64, Option<u64>)]) {
    let help = |o: &mut String, name: &str, kind: &str, h: &str| {
        let _ = writeln!(o, "# HELP {name} {h}\n# TYPE {name} {kind}");
    };
    if let Some(kinds) = resident() {
        help(o, "kiln_memory_resident_bytes", "gauge", "The process's resident memory by kind: anon (heap, stacks, what the allocator keeps), file (the binary's pages read in), shmem.");
        for (kind, bytes) in kinds {
            let _ = writeln!(o, "kiln_memory_resident_bytes{{kind=\"{kind}\"}} {bytes}");
        }
    }
    let ours = OWNERS.iter().chain(owners::ALL.iter()).map(|w| (w.name, w.bytes() as u64, w.bound().map(|b| b as u64)));
    let all: Vec<(&str, u64, Option<u64>)> = ours.chain(others.iter().copied()).collect();
    help(o, "kiln_memory_bytes", "gauge", "Bytes each of the server's caches and buffers holds now, by owner.");
    for (name, bytes, _) in &all {
        let _ = writeln!(o, "kiln_memory_bytes{{owner=\"{name}\"}} {bytes}");
    }
    help(o, "kiln_memory_bound_bytes", "gauge", "The most each owner of kiln_memory_bytes may hold, where that's known.");
    for (name, _, bound) in &all {
        if let Some(b) = bound {
            let _ = writeln!(o, "kiln_memory_bound_bytes{{owner=\"{name}\"}} {b}");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Goal 11, plan 01a1a87b2e56 step 1: the resident memory is said by kind, from `/proc/<pid>/status`.
    #[test]
    fn g11_s1_resident_memory_is_read_by_kind() {
        let status = "Name:\tkiln\nVmRSS:\t  1300 kB\nRssAnon:\t  1000 kB\nRssFile:\t   300 kB\nRssShmem:\t     0 kB\nThreads:\t4\n";
        assert_eq!(resident_of(status), Some([("anon", 1_024_000), ("file", 307_200), ("shmem", 0)]));
        assert_eq!(resident_of("Name:\tkiln\nVmRSS:\t1 kB\n"), None);
    }

    /// Goal 11, plan 01a1a87b2e56 step 3: an owner's count goes up and down and never below none, and its
    /// bound shows once set.
    #[test]
    fn g11_s3_an_owners_count_and_bound_show() {
        static CACHE: Owner = Owner::new("test_cache");
        CACHE.add(300);
        CACHE.sub(100);
        assert_eq!(CACHE.bytes(), 200);
        CACHE.sub(500);
        assert_eq!(CACHE.bytes(), 0);
        assert_eq!(CACHE.bound(), None);
        CACHE.bound_at(1000);
        assert_eq!(CACHE.bound(), Some(1000));
        let mut o = String::new();
        render(&mut o, &[("test_bus", 42, Some(64)), ("test_open", 7, None)]);
        assert!(o.contains("kiln_memory_bytes{owner=\"test_bus\"} 42\n"), "{o}");
        assert!(o.contains("kiln_memory_bound_bytes{owner=\"test_bus\"} 64\n"), "{o}");
        assert!(o.contains("kiln_memory_bytes{owner=\"test_open\"} 7\n"), "{o}");
        assert!(!o.contains("kiln_memory_bound_bytes{owner=\"test_open\"}"), "{o}");
        assert!(o.contains("kiln_memory_bytes{owner=\"ui_assets\"}"), "{o}");
        assert!(o.contains("kiln_memory_bytes{owner=\"snapshots\"}"), "{o}");
    }

    /// Goal 11, plan 01a1a87b2e56 step 3: what's held shows while it's held, goes as it's let go, and what's
    /// left of it goes with its holder however that ends.
    #[test]
    fn g11_s3_held_bytes_go_with_their_holder() {
        static FRAMES: Owner = Owner::new("test_frames");
        let mut held = FRAMES.hold(1000);
        assert_eq!(FRAMES.bytes(), 1000);
        held.release(300);
        assert_eq!(FRAMES.bytes(), 700);
        held.release(5000);
        assert_eq!(FRAMES.bytes(), 0, "never more than it holds");
        let other = FRAMES.hold(250);
        let _again = FRAMES.hold(50);
        drop(other);
        assert_eq!(FRAMES.bytes(), 50, "the rest goes with its holder");
        drop(held);
        assert_eq!(FRAMES.bytes(), 50);
    }
}
