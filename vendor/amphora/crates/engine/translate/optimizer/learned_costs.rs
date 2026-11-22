//! Amphora (QT.5; `docs/design/executor.md`, decision 8): the cost weights of a seek, a hash
//! table's build and probe, and a sort, learned where the engine runs instead of picked. Each is a
//! rate, nanoseconds a unit of its work, kept per database: calibrated once a process from the
//! work itself (one page of keys searched, hashed, probed and sorted), then moved by the times the
//! database's operators take as its queries run. The planner reads each weight as a multiple of
//! the time a row's visit takes, so the four sit on the scale of the costs around them
//! (`CostModelParams::with_learned`). The bench harness `bench/join_methods` checks what they choose;
//! it is not where they come from.

use std::hint::black_box;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::OnceLock;
use std::time::Instant;

use parking_lot::Mutex;
use rustc_hash::FxHashMap;

/// The kinds of work the planner weighs, each timed in its own unit.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Work {
    /// A row visited: read and tested once. The unit the others are weighed in.
    Row,
    /// One search of an ordered index for a key.
    Seek,
    /// One row added to a hash table.
    HashBuild,
    /// One lookup of a hash table.
    HashProbe,
    /// Sorting, in rows times log2 rows, as the planner counts a sort
    /// (`cost::estimate_sort_cpu_cost`).
    Sort,
    /// One page read from the database's file, as the pager times it from the read's submission
    /// to its completion (`sqlite3_ondisk::begin_read_page`). The planner's unit of cost.
    PageRead,
}

/// Units of work and the nanoseconds they took.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
struct Span {
    units: f64,
    nanos: f64,
}

/// One kind's running rate: the work of the epoch under way and of the one before it. An epoch is
/// as much work as calibration timed, so the rate follows the latest one to two calibrations' worth
/// of work and leaves older times behind as conditions move.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
struct Rate {
    epoch: f64,
    previous: Span,
    current: Span,
}

impl Rate {
    /// The rate calibration measured: `units` of work in `nanos`, also the epoch's length.
    fn calibrated(units: f64, nanos: f64) -> Self {
        Self {
            epoch: units,
            previous: Span { units, nanos },
            current: Span::default(),
        }
    }

    /// Adds an observation. One with no work or a time that is not a finite, non-negative number
    /// says nothing about the rate, and is left out.
    fn observe(&mut self, units: f64, nanos: f64) {
        if !(units.is_finite() && units > 0.0 && nanos.is_finite() && nanos >= 0.0) {
            return;
        }
        self.current.units += units;
        self.current.nanos += nanos;
        if self.current.units >= self.epoch {
            self.previous = self.current;
            self.current = Span::default();
        }
    }

    /// Nanoseconds a unit, None before any time was seen.
    fn nanos_per_unit(&self) -> Option<f64> {
        let units = self.previous.units + self.current.units;
        let nanos = self.previous.nanos + self.current.nanos;
        if units > 0.0 && nanos > 0.0 {
            Some(nanos / units)
        } else {
            None
        }
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq)]
struct Rates {
    row: Rate,
    seek: Rate,
    hash_build: Rate,
    hash_probe: Rate,
    sort: Rate,
    page: Rate,
    /// What timing one run costs: a pair of clock reads (step 3a, `Sampling`).
    timing_nanos: f64,
}

impl Rates {
    fn rate_mut(&mut self, work: Work) -> &mut Rate {
        match work {
            Work::Row => &mut self.row,
            Work::Seek => &mut self.seek,
            Work::HashBuild => &mut self.hash_build,
            Work::HashProbe => &mut self.hash_probe,
            Work::Sort => &mut self.sort,
            Work::PageRead => &mut self.page,
        }
    }

    /// The weights, with `miss`, the share of the connection's recent page fetches that read a
    /// file (`Pager::page_miss_ratio`), passed through for the planner.
    fn weights(&self, miss: Option<f64>) -> Option<LearnedWeights> {
        let row = self.row.nanos_per_unit()?;
        Some(LearnedWeights {
            row_in_pages: self.page.nanos_per_unit().map(|page| row / page),
            seek: self.seek.nanos_per_unit()? / row,
            hash_build: self.hash_build.nanos_per_unit()? / row,
            hash_probe: self.hash_probe.nanos_per_unit()? / row,
            sort: self.sort.nanos_per_unit()? / row,
            page_miss: miss.filter(|m| m.is_finite()).map(|m| m.clamp(0.0, 1.0)),
        })
    }
}

/// The four weights, each in rows' visits: what one unit of the work takes against one row read
/// and tested; and the row's visit itself in page reads, the planner's unit, once the database has
/// read a page from its file.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct LearnedWeights {
    /// A row's visit against a page read; None until the database has timed one (a database
    /// whose pages all came from memory so far has no page read to weigh it against).
    pub(crate) row_in_pages: Option<f64>,
    pub(crate) seek: f64,
    pub(crate) hash_build: f64,
    pub(crate) hash_probe: f64,
    pub(crate) sort: f64,
    /// The share of the connection's recent page fetches that read a file, None before its first
    /// fetch (`Pager::page_miss_ratio`): how many of the pages a plan would first read from a cold
    /// cache it reads from the file (`cost::file_reads`).
    pub(crate) page_miss: Option<f64>,
}

/// A database's running cost estimator, shared by its connections.
#[derive(Debug)]
pub(crate) struct LearnedCosts {
    rates: Mutex<Rates>,
    /// Which runs of the seek, the hash build and the hash probe are timed (step 3a).
    seek: Sampling,
    hash_build: Sampling,
    hash_probe: Sampling,
    /// What timing one run costs: two clock reads, as calibration timed them.
    timing_nanos: f64,
}

/// Amphora (QT.5, step 3a): which runs of one operation are timed, one in `every`, and the spread
/// of the times sampled since `every` was last set.
///
/// Timing a run costs `c` (`LearnedCosts::timing_nanos`), so timing one in `k` adds a share
/// `c / (k * mean)` to the operation's work; an epoch of the rate (`n` timed units) is then a
/// mean over `n` samples whose relative standard error is `cv / sqrt(n)`, `cv` the times' spread
/// (standard deviation over mean), and the epoch spans `n * k` runs. `every` is the `k` at which
/// the two are equal over an epoch's runs `n`: `c / (k * mean) = cv * sqrt(k / n)`, that is
/// `k = (c * sqrt(n) / sd)^(2/3)`; at least 1, at most `n`. A steadier operation is timed less
/// often, a costlier timer too; one whose times scatter is timed more. Until a spread is measured,
/// every run is timed.
#[derive(Debug)]
struct Sampling {
    runs: AtomicU64,
    every: AtomicU64,
    spread: Mutex<Spread>,
}

/// Welford's running mean and sum of squared deviations (Welford, "Note on a method for
/// calculating corrected sums of squares and products", Technometrics 4(3), 1962).
#[derive(Debug, Clone, Copy, Default, PartialEq)]
struct Spread {
    count: f64,
    mean: f64,
    m2: f64,
}

impl Spread {
    fn add(&mut self, x: f64) {
        self.count += 1.0;
        let delta = x - self.mean;
        self.mean += delta / self.count;
        self.m2 += delta * (x - self.mean);
    }

    fn sd(&self) -> Option<f64> {
        (self.count > 1.0).then(|| (self.m2 / (self.count - 1.0)).max(0.0).sqrt())
    }
}

impl Sampling {
    fn new() -> Self {
        Self {
            runs: AtomicU64::new(0),
            every: AtomicU64::new(1),
            spread: Mutex::new(Spread::default()),
        }
    }

    fn sampled(&self) -> bool {
        let every = self.every.load(Ordering::Relaxed).max(1);
        self.runs
            .fetch_add(1, Ordering::Relaxed)
            .is_multiple_of(every)
    }

    /// Adds a timed run's `nanos`; once an epoch's `epoch` samples are in, sets `every` from their
    /// spread and `timing_nanos` and starts the next.
    fn add(&self, nanos: f64, epoch: f64, timing_nanos: f64) {
        let mut spread = self.spread.lock();
        spread.add(nanos);
        if spread.count < epoch.max(2.0) {
            return;
        }
        if let Some(every) = sample_every(timing_nanos, epoch, spread.sd()) {
            self.every.store(every, Ordering::Relaxed);
        }
        *spread = Spread::default();
    }
}

/// `k = (c * sqrt(n) / sd)^(2/3)` between 1 and `n` (`Sampling`); None without a spread to go by.
fn sample_every(timing_nanos: f64, epoch: f64, sd: Option<f64>) -> Option<u64> {
    let sd = sd.filter(|sd| sd.is_finite() && *sd > 0.0)?;
    if !(timing_nanos.is_finite() && timing_nanos > 0.0 && epoch.is_finite() && epoch >= 1.0) {
        return None;
    }
    let k = (timing_nanos * epoch.sqrt() / sd).powf(2.0 / 3.0);
    // The nearest whole number of runs.
    Some(k.round().clamp(1.0, epoch) as u64)
}

impl LearnedCosts {
    /// An estimator that starts from this process's calibration.
    pub(crate) fn new() -> Self {
        Self::from_rates(*process_calibration())
    }

    fn from_rates(rates: Rates) -> Self {
        Self {
            rates: Mutex::new(rates),
            seek: Sampling::new(),
            hash_build: Sampling::new(),
            hash_probe: Sampling::new(),
            timing_nanos: rates.timing_nanos,
        }
    }

    fn sampling(&self, work: Work) -> Option<&Sampling> {
        match work {
            Work::Seek => Some(&self.seek),
            Work::HashBuild => Some(&self.hash_build),
            Work::HashProbe => Some(&self.hash_probe),
            Work::Row | Work::Sort | Work::PageRead => None,
        }
    }

    /// Whether to time this run of `work` (step 3a: the seek, the hash build and the hash probe,
    /// one run in their `Sampling`'s `every`).
    pub(crate) fn sampled(&self, work: Work) -> bool {
        self.sampling(work).is_some_and(Sampling::sampled)
    }

    /// Records a timed run of `work`, one unit in `nanos`: it moves the rate, and its spread sets
    /// how often the runs are timed.
    pub(crate) fn observe_sampled(&self, work: Work, nanos: f64) {
        if !(nanos.is_finite() && nanos >= 0.0) {
            return;
        }
        let epoch = {
            let mut rates = self.rates.lock();
            let rate = rates.rate_mut(work);
            rate.observe(1.0, nanos);
            rate.epoch
        };
        if let Some(sampling) = self.sampling(work) {
            sampling.add(nanos, epoch, self.timing_nanos);
        }
    }

    /// Records that `units` of `work` took `nanos`, as an operator timed it.
    pub(crate) fn observe(&self, work: Work, units: f64, nanos: f64) {
        self.rates.lock().rate_mut(work).observe(units, nanos);
    }

    /// Nanoseconds a unit of `work` takes as the rate stands, None before any time was seen: what
    /// G11 step 5's access paths weigh the column copy's measured scan against
    /// (`column_path::RowRates::learned`).
    pub(crate) fn nanos_per(&self, work: Work) -> Option<f64> {
        self.rates.lock().rate_mut(work).nanos_per_unit()
    }

    /// The weights as they stand, no fetch's miss share given; None only if the clock never moved
    /// while calibrating.
    #[cfg(test)]
    pub(crate) fn weights(&self) -> Option<LearnedWeights> {
        self.rates.lock().weights(None)
    }

    /// The weights as they stand for a connection whose recent page fetches read the file `miss`
    /// of the time (`Pager::page_miss_ratio`; None before its first fetch).
    pub(crate) fn weights_at(&self, miss: Option<f64>) -> Option<LearnedWeights> {
        self.rates.lock().weights(miss)
    }

    /// Anchors the page read's rate to `reads` page reads timed at open in `nanos`
    /// (`time_page_read`), before the database has read a page of its own. They count beside the
    /// database's own reads, and the first epoch of those replaces them.
    pub(crate) fn anchor_page_read(&self, reads: f64, nanos: f64) {
        let mut rates = self.rates.lock();
        if !(reads.is_finite() && reads > 0.0 && nanos.is_finite() && nanos > 0.0) {
            return;
        }
        rates.page.previous = Span {
            units: reads,
            nanos,
        };
    }

    /// The page reads an epoch of the page read's rate holds: the most the open's anchor times
    /// (`time_page_read`), as calibration times each kind of work up to a page of it.
    pub(crate) fn page_read_epoch(&self) -> usize {
        self.rates.lock().page.epoch.max(1.0) as usize
    }
}

/// The rates measured once a process, on the hardware it runs on.
fn process_calibration() -> &'static Rates {
    static CALIBRATION: OnceLock<Rates> = OnceLock::new();
    CALIBRATION.get_or_init(|| {
        let start = Instant::now();
        calibrate(&mut || start.elapsed().as_nanos() as f64)
    })
}

/// SplitMix64's output function (Steele, Lea and Flood, "Fast splittable pseudorandom number
/// generators", OOPSLA 2014; its published constants): keys in no order, the same every run.
pub(crate) fn mix(i: u64) -> u64 {
    let mut z = i.wrapping_add(0x9E37_79B9_7F4A_7C15);
    z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    z ^ (z >> 31)
}

/// Times `work` `reps` times over, doubling `reps` up to `most_reps` until the time reaches
/// `target`: (repetitions, nanoseconds).
fn measure(
    clock: &mut dyn FnMut() -> f64,
    target: f64,
    most_reps: usize,
    work: &mut dyn FnMut(),
) -> (f64, f64) {
    let mut reps: usize = 1;
    loop {
        let start = clock();
        for _ in 0..reps {
            work();
        }
        let nanos = clock() - start;
        match reps.checked_mul(2) {
            Some(next) if nanos < target && next <= most_reps => reps = next,
            _ => return (reps as f64, nanos),
        }
    }
}

/// Times each kind of work over one page of 8-byte keys, the unit a b-tree reads
/// (`BufferPool::DEFAULT_PAGE_SIZE`). Each kind is repeated, up to once per key, until its time
/// reaches the slowest kind's single run, so the clock's step weighs on each about equally.
fn calibrate(clock: &mut dyn FnMut() -> f64) -> Rates {
    let n = crate::storage::buffer_pool::BufferPool::DEFAULT_PAGE_SIZE / std::mem::size_of::<u64>();
    let keys: Vec<u64> = (0..n as u64).map(mix).collect();
    let mut ordered = keys.clone();
    ordered.sort_unstable();
    let table: FxHashMap<u64, u64> = keys.iter().map(|k| (*k, *k)).collect();
    let rows = n as f64;

    let mut row = || {
        black_box(keys.iter().filter(|k| black_box(**k) & 1 == 0).count());
    };
    let mut seek = || {
        for k in &keys {
            let _ = black_box(ordered.binary_search(k));
        }
    };
    let mut build = || {
        let mut built = FxHashMap::default();
        for k in &keys {
            built.insert(*k, *k);
        }
        black_box(&built);
    };
    let mut probe = || {
        for k in &keys {
            black_box(table.get(k));
        }
    };
    let mut sort = || {
        let mut sorted = keys.clone();
        sorted.sort_unstable();
        black_box(&sorted);
    };
    let mut kinds: [(Work, f64, &mut dyn FnMut()); 5] = [
        (Work::Row, rows, &mut row),
        (Work::Seek, rows, &mut seek),
        (Work::HashBuild, rows, &mut build),
        (Work::HashProbe, rows, &mut probe),
        (Work::Sort, rows * rows.log2(), &mut sort),
    ];
    let mut target: f64 = 0.0;
    for (_, _, work) in kinds.iter_mut() {
        target = target.max(measure(clock, 0.0, 1, &mut **work).1);
    }
    let mut rates = Rates::default();
    for (kind, units, work) in kinds.iter_mut() {
        let (reps, nanos) = measure(clock, target, n, &mut **work);
        *rates.rate_mut(*kind) = Rate::calibrated(reps * *units, nanos);
    }
    // A pair of clock reads, timed as the kinds are, a page's keys' worth of times at most.
    let mut timing = || {
        let start = Instant::now();
        black_box(start.elapsed());
    };
    let (reps, nanos) = measure(clock, target, n, &mut timing);
    rates.timing_nanos = nanos / reps;
    // A page read can't be timed here: there's no file yet. Each database anchors it at open
    // (`time_page_read`, `LearnedCosts::anchor_page_read`), then its own reads move it
    // (`LearnedCosts::observe`), its epoch the pages whose rows calibration visited.
    rates.page = Rate {
        epoch: (rates.row.epoch / rows).max(1.0),
        ..Rate::default()
    };
    rates
}

/// Times page reads at a database's open, before it has read a page of its own (QT.5): a scratch
/// page of `page_size` bytes, written beside the database through its own IO and read back as its
/// pager reads (`read_through_io`). (reads, nanoseconds), or None if no scratch page could be
/// written there (a database whose directory takes no file is one opened read-only, whose open
/// reads its own file).
///
/// The reads come from the OS's cache, as the database's own reads do while its pages are cached.
/// They are not read uncached (`O_DIRECT`, `F_NOCACHE`, `FILE_FLAG_NO_BUFFERING`): that prices a
/// cached database's page at a device read, which made row visits all but free against it and
/// flipped the planner's plans for databases held in memory (lane qs5-4, kiln run r326 on Windows:
/// three of `tests/planner.rs`'s plans). A database whose reads go to the device teaches the
/// estimator that with its own reads: the first epoch of them replaces these
/// (`LearnedCosts::anchor_page_read`).
///
/// The first read is not the rate: it pays for the IO's first use (on jupiter, Windows, the
/// process's first read took 121.7 us, the reads after it 24.7 to 38.9 us; lane qs5-5's probe,
/// kiln run r558), and a single read anchored on it flipped the key joins of
/// `tests/records_plans.rs`'s `a_join_on_a_whole_key_searches_it_without_statistics` (kiln run
/// r546; it passes with this, run r649). So the reads after it are timed, doubling in number, until
/// their time reaches the first's or they number `most_reads`, as calibration times each kind of
/// work until its time reaches the slowest single run (`measure`).
pub(crate) fn time_page_read(
    db_path: &str,
    io: &crate::sync::Arc<dyn crate::io::IO>,
    page_size: usize,
    most_reads: usize,
) -> Option<(f64, f64)> {
    let page: Vec<u8> = (0..page_size).map(|i| mix(i as u64) as u8).collect();
    let scratch = if crate::database::is_memory_like(db_path) {
        format!(":memory:-page-read-{}", uuid::Uuid::new_v4().simple())
    } else {
        format!("{db_path}-page-read-{}", uuid::Uuid::new_v4().simple())
    };
    match read_through_io(io, &scratch, page, most_reads) {
        Ok(timed) => Some(timed),
        Err(err) => {
            tracing::debug!("QT.5: no scratch page read beside {db_path}: {err}");
            None
        }
    }
}

/// Writes `page` to `scratch` through `io` and reads it back as the pager reads, each read from
/// its submission to its completion: the first read alone, then the reads after it as
/// `time_page_read` says; (those reads, their nanoseconds). The file is removed whatever happens.
fn read_through_io(
    io: &crate::sync::Arc<dyn crate::io::IO>,
    scratch: &str,
    page: Vec<u8>,
    most_reads: usize,
) -> crate::Result<(f64, f64)> {
    use crate::io::{Buffer, Completion, OpenFlags};
    use crate::sync::Arc;
    let file = io.open_file(scratch, OpenFlags::Create, false)?;
    let timed = (|| -> crate::Result<(f64, f64)> {
        let len = page.len();
        let written = file.pwrite(
            0,
            Arc::new(Buffer::new(page)),
            Completion::new_write(|_| {}),
        )?;
        io.wait_for_completion(written)?;
        let read_once = || -> crate::Result<()> {
            let read = file.pread(
                0,
                Completion::new_read(Arc::new(Buffer::new(vec![0; len])), move |res| match res {
                    Ok((_, n)) if n as usize == len => None,
                    Ok((_, n)) => Some(crate::error::CompletionError::ShortRead {
                        page_idx: 0,
                        expected: len,
                        actual: n.max(0) as usize,
                    }),
                    Err(err) => Some(err),
                }),
            )?;
            io.wait_for_completion(read)
        };
        let start = std::time::Instant::now();
        read_once()?;
        let first = start.elapsed().as_nanos() as f64;
        let most_reads = most_reads.max(1);
        let mut reads: usize = 1;
        loop {
            let start = std::time::Instant::now();
            for _ in 0..reads {
                read_once()?;
            }
            let nanos = start.elapsed().as_nanos() as f64;
            match reads.checked_mul(2) {
                Some(next) if nanos < first && next <= most_reads => reads = next,
                _ => return Ok((reads as f64, nanos)),
            }
        }
    })();
    drop(file);
    let removed = io.remove_file(scratch);
    let timed = timed?;
    removed?;
    Ok(timed)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixed_rates(row: f64, seek: f64, build: f64, probe: f64, sort: f64) -> Rates {
        Rates {
            row: Rate::calibrated(100.0, 100.0 * row),
            seek: Rate::calibrated(100.0, 100.0 * seek),
            hash_build: Rate::calibrated(100.0, 100.0 * build),
            hash_probe: Rate::calibrated(100.0, 100.0 * probe),
            sort: Rate::calibrated(100.0, 100.0 * sort),
            page: Rate {
                epoch: 4.0,
                ..Rate::default()
            },
            timing_nanos: 20.0,
        }
    }

    /// Do: a timer of 20 ns, epochs of 100 samples, times spread 25 ns, 2,000 ns and 0.01 ns, and
    /// none. Expect: one run in (20 * 10 / 25)^(2/3) = 4 timed; every run when the times scatter
    /// past the timer's cost; one in the epoch when they barely move; no change without a spread
    /// (step 3a: the sampling follows each operation's spread, no picked rate).
    #[test]
    fn t_qt_5_5_sampling_follows_the_spread() {
        assert_eq!(sample_every(20.0, 100.0, Some(25.0)), Some(4));
        assert_eq!(sample_every(20.0, 100.0, Some(2_000.0)), Some(1));
        assert_eq!(sample_every(20.0, 100.0, Some(0.01)), Some(100));
        assert_eq!(sample_every(20.0, 100.0, Some(0.0)), None);
        assert_eq!(sample_every(20.0, 100.0, None), None);
    }

    /// Do: time every seek of an epoch, 100 ns and 150 ns in turn, then run 120 more. Expect:
    /// every run of the first epoch timed; then some but not all; the seek's rate moved toward
    /// the times observed, the hash probe's untouched (step 3a).
    #[test]
    fn t_qt_5_5_sampled_seeks_move_the_rate_and_thin_out() {
        let costs = LearnedCosts::from_rates(fixed_rates(1.0, 10.0, 20.0, 15.0, 2.0));
        for i in 0..100 {
            assert!(costs.sampled(Work::Seek));
            costs.observe_sampled(Work::Seek, if i % 2 == 0 { 100.0 } else { 150.0 });
        }
        let timed = (0..120).filter(|_| costs.sampled(Work::Seek)).count();
        assert!(timed > 0 && timed < 120, "{timed}");
        let weights = costs.weights().unwrap();
        assert!((weights.seek - 125.0).abs() < 1e-9, "{weights:?}");
        assert_eq!(weights.hash_probe, 15.0);
        assert!(!costs.sampled(Work::Sort));
    }

    /// Do: calibrate on this machine with no operator observed. Expect: every weight a finite,
    /// positive number of rows' visits (T-QT.5.5: the learned costs start sane).
    #[test]
    fn t_qt_5_5_calibration_starts_sane_with_no_observations() {
        let weights = LearnedCosts::new().weights().expect("the clock moved");
        for w in [
            weights.seek,
            weights.hash_build,
            weights.hash_probe,
            weights.sort,
        ] {
            assert!(w.is_finite() && w > 0.0, "{weights:?}");
        }
    }

    /// Do: start from known rates, then observe sorts at four times the calibrated rate for as
    /// much work as two epochs. Expect: the sort weight moves to four times its start, the others
    /// stay (T-QT.5.5: the estimator follows observed times).
    #[test]
    fn t_qt_5_5_observed_times_move_the_weight() {
        let costs = LearnedCosts::from_rates(fixed_rates(1.0, 10.0, 20.0, 15.0, 2.0));
        let start = costs.weights().unwrap();
        assert_eq!(start.sort, 2.0);
        costs.observe(Work::Sort, 50.0, 50.0 * 8.0);
        let partway = costs.weights().unwrap();
        assert!(partway.sort > 2.0 && partway.sort < 8.0, "{partway:?}");
        for _ in 0..4 {
            costs.observe(Work::Sort, 50.0, 50.0 * 8.0);
        }
        let moved = costs.weights().unwrap();
        assert_eq!(moved.sort, 8.0);
        assert_eq!(moved.seek, start.seek);
        assert_eq!(moved.hash_build, start.hash_build);
        assert_eq!(moved.hash_probe, start.hash_probe);
    }

    /// Do: observe nothing-sized and non-finite times. Expect: the weights unchanged (T-QT.5.5).
    #[test]
    fn t_qt_5_5_meaningless_observations_leave_the_rates() {
        let costs = LearnedCosts::from_rates(fixed_rates(1.0, 10.0, 20.0, 15.0, 2.0));
        let start = costs.weights().unwrap();
        costs.observe(Work::Seek, 0.0, 5.0);
        costs.observe(Work::Seek, 10.0, f64::NAN);
        costs.observe(Work::Seek, 10.0, -1.0);
        assert_eq!(costs.weights().unwrap(), start);
    }

    /// Do: a slower row visit. Expect: every weight, counted in rows' visits, falls in proportion
    /// (T-QT.5.5: the weights are relative to the row).
    #[test]
    fn t_qt_5_5_weights_are_in_rows_visits() {
        let fast = LearnedCosts::from_rates(fixed_rates(1.0, 10.0, 20.0, 15.0, 2.0));
        let slow = LearnedCosts::from_rates(fixed_rates(2.0, 10.0, 20.0, 15.0, 2.0));
        let (fast, slow) = (fast.weights().unwrap(), slow.weights().unwrap());
        assert_eq!(slow.seek * 2.0, fast.seek);
        assert_eq!(slow.sort * 2.0, fast.sort);
    }

    /// Do: plan before the database read any page from its file, then after it timed page reads at
    /// 50 rows' visits each, then after reads turn four times slower for two epochs. Expect: no
    /// row-to-page weight at first, then a fiftieth, then a two-hundredth (the anchor learned where
    /// it runs, and followed as the device moves).
    #[test]
    fn t_qt_5_5_the_row_is_weighed_against_observed_page_reads() {
        let costs = LearnedCosts::from_rates(fixed_rates(2.0, 10.0, 20.0, 15.0, 2.0));
        assert_eq!(costs.weights().unwrap().row_in_pages, None);
        costs.observe(Work::PageRead, 1.0, 100.0);
        assert_eq!(costs.weights().unwrap().row_in_pages, Some(2.0 / 100.0));
        for _ in 0..8 {
            costs.observe(Work::PageRead, 1.0, 400.0);
        }
        assert_eq!(costs.weights().unwrap().row_in_pages, Some(2.0 / 400.0));
    }

    /// Do: the weights with no page read timed, and with one, applied to the planner's parameters.
    /// Expect: the row's cost stays the parameters' until a page read is timed, then is the
    /// learned ratio, and every other weight follows it (T-QT.5.5).
    #[test]
    fn t_qt_5_5_the_planner_takes_the_learned_row_cost() {
        let params = super::super::cost_params::CostModelParams::new();
        let costs = LearnedCosts::from_rates(fixed_rates(2.0, 10.0, 20.0, 15.0, 2.0));
        let before = params.with_learned(costs.weights());
        assert_eq!(before.cpu_cost_per_row, params.cpu_cost_per_row);
        costs.observe(Work::PageRead, 1.0, 128.0);
        let after = params.with_learned(costs.weights());
        assert_eq!(after.cpu_cost_per_row, 2.0 / 128.0);
        assert_eq!(after.cpu_cost_per_seek, 10.0 / 128.0);
        assert_eq!(after.hash_insert_cost, 20.0 / 128.0);
    }

    /// Do: weigh with no fetch yet, with a share of 0.25, and with a share out of range. Expect:
    /// the share passed through as the planner's page miss, clamped to [0, 1] (T-QT.5.5).
    #[test]
    fn t_qt_5_5_the_page_miss_share_reaches_the_planner() {
        let costs = LearnedCosts::from_rates(fixed_rates(2.0, 10.0, 20.0, 15.0, 2.0));
        let params = super::super::cost_params::CostModelParams::new();
        assert_eq!(params.with_learned(costs.weights_at(None)).page_miss, 1.0);
        for (miss, want) in [(Some(0.25), 0.25), (Some(3.0), 1.0), (Some(f64::NAN), 1.0)] {
            assert_eq!(params.with_learned(costs.weights_at(miss)).page_miss, want);
        }
    }

    /// Do: anchor the page read at 100 ns, as a database's open does, then read pages of its own
    /// at 400 ns each for one epoch. Expect: the row weighed against the anchor at first, then
    /// against the database's own reads alone (the anchor replaced, no fixed number left).
    #[test]
    fn t_qt_5_5_the_open_anchor_stands_until_the_database_reads() {
        let costs = LearnedCosts::from_rates(fixed_rates(2.0, 10.0, 20.0, 15.0, 2.0));
        costs.anchor_page_read(1.0, 100.0);
        assert_eq!(costs.weights().unwrap().row_in_pages, Some(2.0 / 100.0));
        for _ in 0..4 {
            costs.observe(Work::PageRead, 1.0, 400.0);
        }
        assert_eq!(costs.weights().unwrap().row_in_pages, Some(2.0 / 400.0));
    }

    /// Do: time a scratch page read beside a new database file. Expect: a time, and the
    /// directory left as it was (the scratch page removed).
    #[cfg(feature = "fs")]
    #[test]
    fn t_qt_5_5_a_scratch_page_read_is_timed_beside_the_database() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("anchor.db");
        let io: crate::sync::Arc<dyn crate::io::IO> =
            crate::sync::Arc::new(crate::PlatformIO::new().unwrap());
        let (reads, nanos) = time_page_read(path.to_str().unwrap(), &io, 4096, 8).unwrap();
        assert!((1.0..=8.0).contains(&reads), "{reads}");
        assert!(nanos.is_finite() && nanos > 0.0, "{nanos}");
        assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 0);
    }

    /// Do: time a scratch page read for a database in memory. Expect: a time, read through its
    /// own IO.
    #[test]
    fn t_qt_5_5_a_scratch_page_read_is_timed_in_memory() {
        let io: crate::sync::Arc<dyn crate::io::IO> = crate::sync::Arc::new(crate::MemoryIO::new());
        let (reads, nanos) = time_page_read(":memory:", &io, 4096, 8).unwrap();
        assert!((1.0..=8.0).contains(&reads), "{reads}");
        assert!(nanos.is_finite() && nanos > 0.0, "{nanos}");
    }

    /// Do: open a new, empty database file and ask for its weights before any query. Expect: the
    /// row already weighed in page reads (the open's anchor), not left to a fixed number.
    #[cfg(feature = "fs")]
    #[test]
    fn t_qt_5_5_a_new_database_has_a_page_read_before_its_first_query() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("new.db");
        let io: crate::sync::Arc<dyn crate::io::IO> =
            crate::sync::Arc::new(crate::PlatformIO::new().unwrap());
        let db = crate::Database::open_file(
            io,
            path.to_str().unwrap(),
            crate::sync::Arc::new(crate::SqliteDialect),
        )
        .unwrap();
        let row = db.learned_costs.weights().unwrap().row_in_pages.unwrap();
        assert!(row.is_finite() && row > 0.0, "{row}");
    }

    /// Do: calibrate with a clock that never moves. Expect: no weights, so the planner is told it
    /// has none rather than given zeros (T-QT.5.5).
    #[test]
    fn t_qt_5_5_a_stopped_clock_gives_no_weights() {
        let rates = calibrate(&mut || 0.0);
        assert_eq!(rates.weights(None), None);
    }
}
