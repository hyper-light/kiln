//! The compute pool's awaited jobs by where each was asked for (goal 9, plan 01a1a87b2cec step 5): in kiln's lab
//! (realistic, this tree, 2026-11-06) the pool's completions were 532 of the shards' 910 kicks a second that found a
//! shard parked, 84% of the jobs' pieces under 50 µs, so the handing over may cost more than the job. Each of
//! [`crate::rt::compute`], [`crate::rt::compute_pieces`] and [`crate::rt::compute_map`] takes its caller's place
//! (`#[track_caller]`); a job counts against it its time on the pool's thread (each piece's, where it runs) and the time
//! its asker waited from asking to its result in hand, so what handing it over cost is the difference.
//!
//! **No lock**: a fixed table, an entry's site set once ([`OnceLock`]: a site's first job, which may race another's
//! first for a free entry, the loser going on to the next) and its counts relaxed atomics; a table that is full counts
//! what doesn't fit with no site.

use std::fmt::Write as _;
use std::panic::Location;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

/// Format: entries in the table, as many as hyper-rt's slow polls' (`hyper_rt::slow::ENTRIES`): kiln asks for compute
/// jobs from a few dozen places, so a site past this many is counted with no site rather than evicting one.
pub const ENTRIES: usize = hyper_rt::slow::ENTRIES;

/// One entry: a site set once, its jobs, their time on the pool's threads, and their askers' waits.
#[derive(Debug, Default)]
struct Entry {
    site: OnceLock<&'static Location<'static>>,
    jobs: AtomicU64,
    run_ns: AtomicU64,
    waited_ns: AtomicU64,
}

/// The table (see the module's notes).
#[derive(Debug)]
pub struct Sites {
    entries: Box<[Entry]>,
    rest: Entry,
}

impl Default for Sites {
    fn default() -> Self {
        Self { entries: (0..ENTRIES).map(|_| Entry::default()).collect(), rest: Entry::default() }
    }
}

/// The entry `site`'s probe starts at, of `n`: its line and column mixed by Knuth's multiplicative hash.
fn start(site: &Location<'_>, n: usize) -> usize {
    /// Format: 2^64 over the golden ratio.
    const GOLDEN: u64 = 0x9e37_79b9_7f4a_7c15;
    let key = u64::from(site.line()).wrapping_shl(32) | u64::from(site.column());
    usize::try_from(key.wrapping_mul(GOLDEN).wrapping_shr(32)).ok().and_then(|m| m.checked_rem(n)).unwrap_or(0)
}

/// One site's figures as read.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Site {
    /// Where its jobs were asked for; none for what a full table had no entry for.
    pub site: Option<&'static Location<'static>>,
    pub jobs: u64,
    /// Their time on the pool's threads, nanoseconds.
    pub run_ns: u64,
    /// Their askers' waits, from asking to the result in hand, nanoseconds.
    pub waited_ns: u64,
}

impl Sites {
    /// `site`'s entry, claimed where it has none yet; the rest where the table is full.
    fn entry(&self, site: &'static Location<'static>) -> &Entry {
        let n = self.entries.len();
        let first = start(site, n);
        for i in 0..n {
            let Some(entry) = first.wrapping_add(i).checked_rem(n).and_then(|at| self.entries.get(at)) else { continue };
            if entry.site.get().is_none() {
                let _ = entry.site.set(site);
            }
            if entry.site.get().is_some_and(|s| *s == site) {
                return entry;
            }
        }
        &self.rest
    }

    /// A job of `site`'s ran `run` on a pool's thread (one piece of it, where it runs in pieces): the counts' own call.
    pub fn ran(&self, site: &'static Location<'static>, run: Duration) {
        self.entry(site).run_ns.fetch_add(nanos(run), Ordering::Relaxed);
    }

    /// A job of `site`'s was asked for and its result taken `waited` later.
    pub fn waited(&self, site: &'static Location<'static>, waited: Duration) {
        let entry = self.entry(site);
        entry.waited_ns.fetch_add(nanos(waited), Ordering::Relaxed);
        entry.jobs.fetch_add(1, Ordering::Relaxed);
    }

    /// Every site with a job counted, then the rest, as they stand.
    pub fn read(&self) -> Vec<Site> {
        let read = |entry: &Entry| Site {
            site: entry.site.get().copied(),
            jobs: entry.jobs.load(Ordering::Relaxed),
            run_ns: entry.run_ns.load(Ordering::Relaxed),
            waited_ns: entry.waited_ns.load(Ordering::Relaxed),
        };
        self.entries.iter().map(read).chain(std::iter::once(read(&self.rest))).filter(|s| s.jobs > 0 || s.run_ns > 0).collect()
    }
}

fn nanos(d: Duration) -> u64 {
    u64::try_from(d.as_nanos()).unwrap_or(u64::MAX)
}

/// The process's table.
pub fn sites() -> &'static Sites {
    static SITES: OnceLock<Sites> = OnceLock::new();
    SITES.get_or_init(Sites::default)
}

/// `kiln_compute_site_*{site}`: the awaited jobs by where each was asked for, their time on the pool's threads and their
/// askers' waits (from asking to the result in hand), in the exposition.
pub fn render(o: &mut String) {
    series(o, &sites().read());
}

fn series(o: &mut String, all: &[Site]) {
    #[allow(clippy::cast_precision_loss)]
    let secs = |ns: u64| ns as f64 / 1e9;
    let site = |s: &Site| s.site.map_or_else(String::new, |l| format!("{}:{}:{}", l.file(), l.line(), l.column()));
    let _ = writeln!(o, "# HELP kiln_compute_site_jobs_total The compute pool's awaited jobs, by where each was asked for (file:line:column).\n# TYPE kiln_compute_site_jobs_total counter");
    for s in all {
        let _ = writeln!(o, "kiln_compute_site_jobs_total{{site=\"{}\"}} {}", site(s), s.jobs);
    }
    let _ = writeln!(o, "# HELP kiln_compute_site_run_seconds_total Their time on the pool's threads.\n# TYPE kiln_compute_site_run_seconds_total counter");
    for s in all {
        let _ = writeln!(o, "kiln_compute_site_run_seconds_total{{site=\"{}\"}} {}", site(s), secs(s.run_ns));
    }
    let _ = writeln!(o, "# HELP kiln_compute_site_waited_seconds_total Their askers' waits, from asking to the result in hand: less their run, what handing them over cost.\n# TYPE kiln_compute_site_waited_seconds_total counter");
    for s in all {
        let _ = writeln!(o, "kiln_compute_site_waited_seconds_total{{site=\"{}\"}} {}", site(s), secs(s.waited_ns));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[track_caller]
    fn here() -> &'static Location<'static> {
        Location::caller()
    }

    /// Goal 9, plan 01a1a87b2cec step 5: jobs are counted by site with their run and their askers' waits, a site its own
    /// entry; on `/metrics` a series each.
    #[test]
    fn g9_s5_compute_jobs_count_by_site() {
        let table = Sites::default();
        let a = here();
        let b = here();
        table.ran(a, Duration::from_micros(10));
        table.waited(a, Duration::from_micros(40));
        table.ran(a, Duration::from_micros(20));
        table.waited(a, Duration::from_micros(50));
        table.ran(b, Duration::from_micros(5));
        table.waited(b, Duration::from_micros(9));
        let mut read = table.read();
        read.sort_by_key(|s| std::cmp::Reverse(s.jobs));
        assert_eq!(read, vec![Site { site: Some(a), jobs: 2, run_ns: 30_000, waited_ns: 90_000 }, Site { site: Some(b), jobs: 1, run_ns: 5_000, waited_ns: 9_000 }]);
        let mut o = String::new();
        series(&mut o, &read);
        let at = format!("{}:{}:{}", a.file(), a.line(), a.column());
        for line in [format!("kiln_compute_site_jobs_total{{site=\"{at}\"}} 2"), format!("kiln_compute_site_run_seconds_total{{site=\"{at}\"}} 0.00003"), format!("kiln_compute_site_waited_seconds_total{{site=\"{at}\"}} 0.00009")] {
            assert!(o.lines().any(|l| l == line), "{line:?} not in:\n{o}");
        }
    }
}
