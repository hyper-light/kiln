//! The producer's shared half: its plan (SEEDABLE), the census over a driver's guests, each read at every
//! boundary of the node's CPU-control window from its VMM's launch through its readiness and an idle hold,
//! the cost they derive, its evidence, the samples it rests on, and the output.

use std::fmt::Write as _;
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, TryRecvError};
use std::time::{Duration, Instant};

use hex_api::{CapacityResources, RuntimeCost};
use sha2::{Digest, Sha256};

use super::seam::{Driver, Files, Held, Holding, Probe, Use};

/// The census whose rule names the fewest samples a row seeds from (decision 9).
const CENSUS: &str = include_str!("../../../../bench/competitive/probes/waits_census.py");

/// The evidence body's first line, after any CPU-response section: what reads it knows it by.
const HEADER: &str = "hex-guest-cost 2";

pub struct Plan {
    /// The census rule's SEEDABLE: the fewest guests a row is taken from.
    pub seedable: usize,
}

impl Plan {
    pub fn derived() -> Result<Self, String> {
        let seedable = CENSUS
            .lines()
            .find_map(|line| line.strip_prefix("SEEDABLE = "))
            .and_then(|n| n.trim().replace('_', "").parse::<usize>().ok())
            .filter(|n| *n > 0)
            .ok_or("bench/competitive/probes/waits_census.py names no SEEDABLE")?;
        Ok(Self { seedable })
    }
}

/// The window the node's CPU control enforces a VM's rate over (Linux: its cgroup's `cpu.max` period;
/// macOS: the managed CPU control's cycle), and where it comes from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Window {
    pub length: Duration,
    pub source: String,
}

/// The windows a VM's rate is enforced over: one period whatever the rate (Linux's `cpu.max`), or one that
/// the VM's rate sets (the Mac control's cycle), given that rate in milli-CPU.
pub enum Windows {
    #[cfg(any(target_os = "linux", test))]
    Fixed(Window),
    #[cfg(any(target_os = "macos", test))]
    AtRate(Box<dyn Fn(u64) -> Result<Window, String>>),
}

impl Windows {
    pub fn at(&self, rate: u64) -> Result<Window, String> {
        match self {
            #[cfg(any(target_os = "linux", test))]
            Self::Fixed(window) => {
                let _ = rate;
                Ok(window.clone())
            }
            #[cfg(any(target_os = "macos", test))]
            Self::AtRate(at) => at(rate),
        }
    }

    pub fn depends_on_rate(&self) -> bool {
        match self {
            #[cfg(any(target_os = "linux", test))]
            Self::Fixed(_) => false,
            #[cfg(any(target_os = "macos", test))]
            Self::AtRate(_) => true,
        }
    }
}

/// One reading, `at` after its VMM's launch.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Tick<R> {
    pub at: Duration,
    pub reading: R,
}

/// A ready guest's readings: its CPU's at each window boundary, and what it held, read apart.
pub struct Readings {
    pub cpu: Vec<Tick<Use>>,
    pub holdings: Vec<Tick<Holding>>,
}

/// What the census tells a sampling thread.
#[derive(Debug, Clone, Copy)]
enum Told {
    /// Read through this window boundary, then end.
    Until(u32),
    /// End now: the guest was not ready, or its measure failed.
    Abandon,
}

/// The sampling threads' clock: a wait to an instant on a timer the OS doesn't coalesce. macOS coalesces an
/// ordinary timed wait (on this lane's Mac, waits for a 4.3 ms window woke a median 8.8 ms and up to 34 ms
/// late), so it waits as the managed CPU control's own watcher does, on a one-shot kqueue timer marked
/// `NOTE_CRITICAL` (`hex_runtime::macos::sys::Kqueue::wait`). Linux's timed waits are hrtimers, late by
/// their thread's timer slack alone.
struct Clock {
    #[cfg(target_os = "macos")]
    kq: hex_runtime::macos::sys::Kqueue,
    #[cfg(target_os = "macos")]
    events: Vec<hex_runtime::macos::sys::Event>,
}

impl Clock {
    fn new() -> Result<Self, String> {
        Ok(Self {
            #[cfg(target_os = "macos")]
            kq: hex_runtime::macos::sys::Kqueue::new().map_err(|e| format!("a kqueue: {e}"))?,
            #[cfg(target_os = "macos")]
            events: Vec::new(),
        })
    }

    /// Returns at `due` or just after, `bound` at most a wait (a window: `due` is never further off).
    fn until(&mut self, due: Instant, bound: Duration) -> Result<(), String> {
        while let Some(left) = due
            .checked_duration_since(Instant::now())
            .filter(|left| !left.is_zero())
        {
            #[cfg(target_os = "macos")]
            self.kq
                .wait(Some(left), Some(bound), &mut self.events)
                .map_err(|e| format!("the sampling timer: {e}"))?;
            #[cfg(target_os = "linux")]
            {
                let _ = bound;
                std::thread::park_timeout(left);
            }
        }
        Ok(())
    }
}

/// Reads `probe` at each `window` boundary after `began` (the VMM's launch) through the one it is told,
/// taking the census's words at each boundary. Each reading is stamped as it starts; a boundary passed while
/// this thread read or was held up is passed over, so a window is longer after a stall, never shorter, and
/// the next is the next boundary to come.
fn sample<P: Probe>(
    mut probe: P,
    began: Instant,
    window: Duration,
    told: &mpsc::Receiver<Told>,
) -> Result<Vec<Tick<P::Reading>>, String> {
    let mut clock = Clock::new()?;
    let mut ticks = Vec::new();
    let mut last = None;
    let mut next: u32 = 1;
    loop {
        let due = window
            .checked_mul(next)
            .and_then(|span| began.checked_add(span))
            .ok_or("a window boundary overflows the clock")?;
        clock.until(due, window)?;
        loop {
            match told.try_recv() {
                Ok(Told::Until(until)) => last = Some(until),
                Ok(Told::Abandon) | Err(TryRecvError::Disconnected) => return Ok(ticks),
                Err(TryRecvError::Empty) => break,
            }
        }
        if last.is_some_and(|last| next > last) {
            probe.verify()?;
            return Ok(ticks);
        }
        let at = began.elapsed();
        let reading = probe.read()?;
        ticks.push(Tick { at, reading });
        let passed = u32::try_from(began.elapsed().as_nanos() / window.as_nanos())
            .map_err(|_| "a guest outlasts the windows a count holds".to_owned())?;
        next = next.max(passed).saturating_add(1);
        if last.is_some_and(|last| next > last) {
            probe.verify()?;
            return Ok(ticks);
        }
    }
}

/// A sampling thread of `probe` from `began`, told through the returned sender.
type Sampler<R> = (
    mpsc::Sender<Told>,
    std::thread::JoinHandle<Result<Vec<Tick<R>>, String>>,
);

fn sampler<P: Probe>(
    name: &str,
    probe: P,
    began: Instant,
    window: Duration,
) -> std::io::Result<Sampler<P::Reading>> {
    let (tell, told) = mpsc::channel();
    let thread = std::thread::Builder::new()
        .name(name.to_owned())
        .spawn(move || sample(probe, began, window, &told))?;
    Ok((tell, thread))
}

/// Its readings, once its thread ends.
fn joined<R>(sampler: Sampler<R>) -> Result<Vec<Tick<R>>, String> {
    let (tell, thread) = sampler;
    let ticks = thread
        .join()
        .map_err(|_| "a sampling thread panicked".to_owned())
        .and_then(|ticks| ticks);
    drop(tell);
    ticks
}

/// The boundary a ready guest's idle hold ends on: held as long as its boot took and at least one whole
/// window after its readiness, to a whole window.
fn hold_end(ready: Duration, window: Duration) -> Result<u32, String> {
    let span = ready
        .checked_add(ready.max(window))
        .ok_or("a hold overflows the clock")?;
    u32::try_from(span.as_nanos().div_ceil(window.as_nanos()))
        .map_err(|_| "a hold outlasts the windows a count holds".to_owned())
}

fn after(from: Instant, span: Duration) -> Result<Instant, String> {
    from.checked_add(span)
        .ok_or_else(|| format!("a wait of {span:?} overflows the clock"))
}

/// What came of one launched guest.
enum Boot {
    RanOut,
    Ready {
        ready: Duration,
        readings: Readings,
        free: CapacityResources,
    },
}

/// Launches one guest, reads it per window from its launch through its readiness (within `bound`) and its
/// idle hold, has its agent report, and tears it down whatever came of the rest: no guest or sampling
/// thread outlives this.
fn measure<D: Driver>(driver: &mut D, bound: Duration, window: Duration) -> Result<Boot, String> {
    let began = Instant::now();
    let (mut guest, cpu, holds) = driver.launch(after(began, bound)?)?;
    let started = match sampler("guest-cost-cpu", cpu, began, window) {
        Ok(cpu) => match sampler("guest-cost-holdings", holds, began, window) {
            Ok(holds) => Ok((cpu, holds)),
            Err(e) => {
                let _ = cpu.0.send(Told::Abandon);
                let _ = joined(cpu);
                Err(e)
            }
        },
        Err(e) => Err(e),
    };
    let (cpu, holds) = match started {
        Ok(samplers) => samplers,
        Err(e) => {
            let ended = after(Instant::now(), bound).and_then(|at| driver.tear_down(guest, at));
            return Err(format!(
                "starting a sampling thread: {e}{}",
                ended
                    .err()
                    .map(|e| format!("; then {e}"))
                    .unwrap_or_default()
            ));
        }
    };
    let held = (|| -> Result<Option<Duration>, String> {
        if !driver.ready(&mut guest, after(began, bound)?)? {
            return Ok(None);
        }
        let ready = began.elapsed();
        let until = hold_end(ready, window)?;
        // A refused word means its sampler has ended: its own error comes with its join below.
        let _ = cpu.0.send(Told::Until(until));
        let _ = holds.0.send(Told::Until(until));
        let end = window
            .checked_mul(until)
            .ok_or("a hold overflows the clock")?;
        driver.idle(&guest, after(began, end)?)?;
        Ok(Some(ready))
    })();
    if !matches!(held, Ok(Some(_))) {
        let _ = cpu.0.send(Told::Abandon);
        let _ = holds.0.send(Told::Abandon);
    }
    let readings =
        joined(cpu).and_then(|cpu| joined(holds).map(|holdings| Readings { cpu, holdings }));
    let reported = match (&held, &readings) {
        (Ok(Some(_)), Ok(_)) => {
            Some(after(Instant::now(), bound).and_then(|at| driver.report(&guest, at)))
        }
        _ => None,
    };
    let ended = after(Instant::now(), bound).and_then(|at| driver.tear_down(guest, at));
    let boot = match (held, readings, reported) {
        (Ok(None), _, _) => Ok(Boot::RanOut),
        (Ok(Some(ready)), Ok(readings), Some(Ok(free))) => Ok(Boot::Ready {
            ready,
            readings,
            free,
        }),
        (Err(e), _, _) | (Ok(Some(_)), Err(e), _) | (Ok(Some(_)), Ok(_), Some(Err(e))) => Err(e),
        (Ok(Some(_)), Ok(_), None) => Err("the guest's report was never asked".to_owned()),
    };
    match (boot, ended) {
        (Ok(boot), Ok(())) => Ok(boot),
        (Ok(_), Err(e)) => Err(e),
        (Err(e), Ok(())) => Err(e),
        (Err(e), Err(end)) => Err(format!("{e}; then {end}")),
    }
}

/// One ready guest's measure: the most of each over its readings.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Sample {
    /// How long it took from its VMM's launch to its agent answering.
    pub ready: Duration,
    /// Its CPU's windows, and its longest (a boundary read late after a stall makes one longer).
    pub windows: usize,
    pub longest: Duration,
    /// Its VMM's own CPU in one window from its readiness on (its steady state, idle), its guest's left out,
    /// in milli-CPU rounded up: what is charged.
    pub cpu_milli: u64,
    /// The same in one window before its readiness: its boot's burst, not charged.
    pub boot_milli: u64,
    /// Its VMM's whole CPU in one window, its guest's included, for the record.
    pub total_milli: u64,
    /// Its readings of what it held.
    pub holdings: usize,
    /// Its VMM's own memory: its footprint less its guest's touched pages, at its most over the readings from
    /// its readiness on (production's state: the guest ready, its node agent running).
    pub mem_bytes: u64,
    /// The page cache its OS charged it apart, at its most over its readings: recorded, not charged.
    pub file_bytes: u64,
    /// Its readings whose footprint was below their guest RAM's pages (macOS's ledger charges the pages a
    /// guest touches after the region holds them, in its boot): each counted none of the VMM's own.
    pub behind: usize,
    /// The bytes held in its own folders: there during its life, neither before it nor after it.
    pub disk_bytes: u64,
    /// What its agent reported free.
    pub free: CapacityResources,
}

/// `used` over `span` as milli-CPU, rounded up.
fn milli(used: Duration, span: Duration) -> Result<u64, String> {
    if span.is_zero() {
        return Err("a window of no time".to_owned());
    }
    let milli = used
        .as_nanos()
        .checked_mul(1000)
        .map(|n| n.div_ceil(span.as_nanos()))
        .ok_or("a VMM's CPU overflows its rate")?;
    u64::try_from(milli).map_err(|_| "a VMM's CPU rate overflows".to_owned())
}

/// What is held on disk under `folders`: every path's file and allocated bytes (`st_blocks` x 512, a
/// hard-linked file once), sorted by path, walked with an explicit stack; a path gone between its listing
/// and its read is passed over, as a folder not there at all.
pub fn walk(folders: &[PathBuf]) -> Result<Files, String> {
    use std::io::ErrorKind::NotFound;
    use std::os::unix::fs::MetadataExt as _;
    let mut held = Vec::new();
    let mut seen = std::collections::BTreeSet::new();
    let mut stack = folders.to_vec();
    while let Some(path) = stack.pop() {
        let meta = match std::fs::symlink_metadata(&path) {
            Ok(meta) => meta,
            Err(e) if e.kind() == NotFound => continue,
            Err(e) => return Err(format!("reading {}: {e}", path.display())),
        };
        if meta.is_dir() {
            let entries = match std::fs::read_dir(&path) {
                Ok(entries) => entries,
                Err(e) if e.kind() == NotFound => continue,
                Err(e) => return Err(format!("listing {}: {e}", path.display())),
            };
            for entry in entries {
                match entry {
                    Ok(entry) => stack.push(entry.path()),
                    Err(e) if e.kind() == NotFound => {}
                    Err(e) => return Err(format!("listing {}: {e}", path.display())),
                }
            }
        }
        let file = (meta.dev(), meta.ino());
        if seen.insert(file) {
            held.push(Held {
                path,
                file,
                bytes: meta.blocks().saturating_mul(512),
            });
        }
    }
    held.sort();
    Ok(held)
}

/// Whether `path` is listed in `files`, which is sorted by path.
fn listed(files: &Files, path: &Path) -> bool {
    files
        .binary_search_by(|held| held.path.as_path().cmp(path))
        .is_ok()
}

/// The bytes of `files` held by the guest alone: files neither held `before` its launch nor `after` its end,
/// wherever they were (a cache written under one name and renamed is the cache it became).
fn own_disk(files: &Files, before: &Files, after: &Files) -> u64 {
    let kept: std::collections::BTreeSet<(u64, u64)> =
        before.iter().chain(after).map(|held| held.file).collect();
    files
        .iter()
        .filter(|held| !kept.contains(&held.file))
        .map(|held| held.bytes)
        .fold(0, u64::saturating_add)
}

/// A ready guest's measure from its readings, what its agent reported, and what its folders held before its
/// launch and after its end. Its CPU is its VMM's less its guest's, each window's from the reading before
/// (the first from its launch, when its VMM had used nothing) over the window or, read late, over its whole
/// span: a reading never charges a window shorter than the window. A window that began before its readiness
/// is its boot's; one that began at or after it, its steady state's, of which there must be one. Its vCPU
/// threads at its last CPU reading are the envelope's where its OS names them. Its memory is as
/// [`Sample::mem_bytes`] says, its guest's RAM told apart by its last reading of what it held (a reading
/// whose footprint is below its guest's RAM counts none, and is counted); its page cache is recorded apart.
pub fn measured(
    ready: Duration,
    readings: &Readings,
    free: CapacityResources,
    window: Duration,
    vcpus: u32,
    before: &Files,
    after: &Files,
) -> Result<Sample, String> {
    let last = readings
        .cpu
        .last()
        .ok_or("a ready guest with no CPU reading")?;
    if let Some(threads) = last.reading.guest_threads
        && threads != vcpus
    {
        return Err(format!(
            "its VMM shows {threads} vCPU threads; its guest has {vcpus}"
        ));
    }
    let held = readings
        .holdings
        .last()
        .ok_or("a ready guest with no reading of what it held")?;
    if held.reading.guest_memory.is_none() {
        return Err("the guest's RAM was never told apart in its VMM's memory".to_owned());
    }
    let mut sample = Sample {
        ready,
        windows: readings.cpu.len(),
        longest: Duration::ZERO,
        cpu_milli: 0,
        boot_milli: 0,
        total_milli: 0,
        holdings: readings.holdings.len(),
        mem_bytes: 0,
        file_bytes: 0,
        behind: 0,
        disk_bytes: 0,
        free,
    };
    let mut steady = 0usize;
    let (mut at, mut cpu, mut own) = (Duration::ZERO, Duration::ZERO, Duration::ZERO);
    for tick in &readings.cpu {
        let reading = &tick.reading;
        let span = tick.at.checked_sub(at).ok_or("readings out of order")?;
        sample.longest = sample.longest.max(span);
        let charged = span.max(window);
        let now_own = reading.cpu.saturating_sub(reading.guest_cpu);
        let used = milli(now_own.saturating_sub(own), charged)?;
        if at >= ready {
            steady = steady.saturating_add(1);
            sample.cpu_milli = sample.cpu_milli.max(used);
        } else {
            sample.boot_milli = sample.boot_milli.max(used);
        }
        sample.total_milli = sample
            .total_milli
            .max(milli(reading.cpu.saturating_sub(cpu), charged)?);
        // Counters read in whole ticks can read a little lower than before: a dip is charged once, never twice.
        (at, cpu, own) = (tick.at, cpu.max(reading.cpu), own.max(now_own));
    }
    if steady == 0 {
        return Err("no window began after its readiness".to_owned());
    }
    let mut settled = 0usize;
    for tick in &readings.holdings {
        let reading = &tick.reading;
        let guest = reading.guest_memory.unwrap_or(0);
        if guest > reading.footprint {
            sample.behind = sample.behind.saturating_add(1);
        }
        if tick.at >= ready {
            settled = settled.saturating_add(1);
            sample.mem_bytes = sample
                .mem_bytes
                .max(reading.footprint.saturating_sub(guest));
        }
        sample.file_bytes = sample.file_bytes.max(reading.file_pages.unwrap_or(0));
        sample.disk_bytes = sample
            .disk_bytes
            .max(own_disk(&reading.files, before, after));
    }
    if settled == 0 {
        return Err("no reading of what it held after its readiness".to_owned());
    }
    Ok(sample)
}

/// The samples file: every ready guest's readings, the evidence's to rest on, hashed as written.
pub struct Record {
    path: PathBuf,
    file: std::io::BufWriter<std::fs::File>,
    hash: Sha256,
}

impl Record {
    pub fn create(path: PathBuf) -> Result<Self, String> {
        let file =
            std::fs::File::create(&path).map_err(|e| format!("making {}: {e}", path.display()))?;
        let mut record = Self {
            path,
            file: std::io::BufWriter::new(file),
            hash: Sha256::new(),
        };
        record.line("# hex-guest-cost samples 2")?;
        record.line("# a census at a window: \"census <k> window_ns <ns> rate_cpu_milli <r>\", then its guests")?;
        record.line("# a ready guest: \"guest <n> ready_ns <ns>\", then its readings, each stamped from its VMM's launch:")?;
        record.line("#   cpu at_ns cpu_ns guest_cpu_ns guest_threads|-")?;
        record.line("#   held at_ns footprint_bytes file_pages_bytes|- peak_footprint_bytes|- guest_memory_bytes|- own_disk_bytes held_disk_bytes")?;
        record.line("# a guest that ran out: \"ran_out <n> bound_ns <ns>\"")?;
        Ok(record)
    }

    fn line(&mut self, text: &str) -> Result<(), String> {
        self.hash.update(text.as_bytes());
        self.hash.update(b"\n");
        writeln!(self.file, "{text}").map_err(|e| format!("writing {}: {e}", self.path.display()))
    }

    fn guest(
        &mut self,
        n: usize,
        ready: Duration,
        readings: &Readings,
        before: &Files,
        after: &Files,
    ) -> Result<(), String> {
        let dash = |value: Option<u64>| value.map_or_else(|| "-".to_owned(), |v| v.to_string());
        self.line(&format!("guest {n} ready_ns {}", ready.as_nanos()))?;
        for tick in &readings.cpu {
            let r = &tick.reading;
            self.line(&format!(
                "cpu {} {} {} {}",
                tick.at.as_nanos(),
                r.cpu.as_nanos(),
                r.guest_cpu.as_nanos(),
                dash(r.guest_threads.map(u64::from)),
            ))?;
        }
        for tick in &readings.holdings {
            let r = &tick.reading;
            let held = r
                .files
                .iter()
                .map(|held| held.bytes)
                .fold(0, u64::saturating_add);
            self.line(&format!(
                "held {} {} {} {} {} {} {held}",
                tick.at.as_nanos(),
                r.footprint,
                dash(r.file_pages),
                dash(r.peak_footprint),
                dash(r.guest_memory),
                own_disk(&r.files, before, after),
            ))?;
        }
        Ok(())
    }

    pub fn finish(mut self) -> Result<(PathBuf, [u8; 32]), String> {
        self.file
            .flush()
            .map_err(|e| format!("writing {}: {e}", self.path.display()))?;
        Ok((self.path, self.hash.finalize().into()))
    }
}

/// A run's guests at one window: each that was ready, the bound each one that ran out passed, and what its
/// first guest left on disk after its end (the run's first census's alone may: what later guests found there
/// and left as it was is the host's one-time caches, no one VM's).
pub struct Census {
    pub first: Duration,
    pub samples: Vec<Sample>,
    pub ran_out: Vec<Duration>,
    pub caches: Files,
}

/// The paths of `after` that `before` didn't hold.
fn new_paths(before: &Files, after: &Files) -> Files {
    after
        .iter()
        .filter(|held| !listed(before, &held.path))
        .cloned()
        .collect()
}

/// Launches guests one at a time until `seedable` were ready. Each is booted within the bound, which starts
/// at `first` and doubles at each run-out (decision 9's run-outs, each recorded), read per window from its
/// launch through its readiness and an idle hold, reported on and torn down. A run whose run-outs reach
/// `seedable` is refused: its class's guest runs out as often as the census needs answers. Only the run's
/// first guest (`caches`: this census's first) may leave anything new on disk; any other that does is
/// refused: a VM's leftovers are no one-time cache.
pub fn census<D: Driver>(
    driver: &mut D,
    seedable: usize,
    first: Duration,
    window: &Window,
    vcpus: u32,
    caches: bool,
    record: &mut Record,
) -> Result<Census, String> {
    let mut census = Census {
        first,
        samples: Vec::with_capacity(seedable),
        ran_out: Vec::new(),
        caches: Vec::new(),
    };
    let mut known = driver.held()?;
    let mut launched = 0usize;
    let mut bound = first;
    while census.samples.len() < seedable {
        if census.ran_out.len() >= seedable {
            return Err(format!(
                "{} boots ran out (the last at {bound:?}) before {seedable} were ready",
                census.ran_out.len()
            ));
        }
        let boot = measure(driver, bound, window.length)?;
        let after = driver.held()?;
        let left = new_paths(&known, &after);
        if caches && launched == 0 {
            census.caches = left;
        } else if !left.is_empty() {
            let named: Vec<String> = left
                .iter()
                .map(|held| format!("{} ({} bytes)", held.path.display(), held.bytes))
                .collect();
            return Err(format!(
                "guest {launched} left {} on disk after its end: a VM's leftovers, never a one-time cache",
                named.join(", ")
            ));
        }
        let n = launched;
        launched = launched.saturating_add(1);
        match boot {
            Boot::RanOut => {
                record.line(&format!(
                    "ran_out {} bound_ns {}",
                    census.ran_out.len(),
                    bound.as_nanos()
                ))?;
                census.ran_out.push(bound);
                bound = bound
                    .checked_mul(2)
                    .ok_or("the doubled boot bound overflows")?;
            }
            Boot::Ready {
                ready,
                readings,
                free,
            } => {
                let sample = measured(ready, &readings, free, window.length, vcpus, &known, &after)
                    .map_err(|e| format!("guest {n}: {e}"))?;
                record.guest(census.samples.len(), ready, &readings, &known, &after)?;
                census.samples.push(sample);
            }
        }
        known = after;
    }
    Ok(census)
}

/// The census's measured parts, each its maximum (its rule's recorded maximum) and each dimension apart.
/// Overhead, outside the envelope: the VMM's own CPU in one window of its steady state, its own memory, and
/// the bytes held in its own folders. Guest overhead, inside: the envelope less what the guest's agent
/// reported free. And the boot's burst, the VMM's own CPU in one window before its guest was ready.
pub fn derive(
    envelope: CapacityResources,
    census: &Census,
    seedable: usize,
) -> Result<(CapacityResources, CapacityResources, u64), String> {
    if census.samples.len() < seedable {
        return Err(format!(
            "{} guests measured, fewer than SEEDABLE {seedable}",
            census.samples.len()
        ));
    }
    let mut overhead = CapacityResources::default();
    let mut inside = CapacityResources::default();
    let mut boot = 0u64;
    for sample in &census.samples {
        overhead.cpu_milli = overhead.cpu_milli.max(sample.cpu_milli);
        overhead.mem_bytes = overhead.mem_bytes.max(sample.mem_bytes);
        overhead.disk_bytes = overhead.disk_bytes.max(sample.disk_bytes);
        boot = boot.max(sample.boot_milli);
        inside.cpu_milli = inside
            .cpu_milli
            .max(envelope.cpu_milli.saturating_sub(sample.free.cpu_milli));
        inside.mem_bytes = inside
            .mem_bytes
            .max(envelope.mem_bytes.saturating_sub(sample.free.mem_bytes));
        inside.disk_bytes = inside
            .disk_bytes
            .max(envelope.disk_bytes.saturating_sub(sample.free.disk_bytes));
    }
    Ok((overhead, inside, boot))
}

pub fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// What a run wrote.
pub struct Made {
    pub summary: String,
    pub cost_file: PathBuf,
    pub cost_sha256: [u8; 32],
    pub evidence_file: PathBuf,
    pub samples_file: PathBuf,
}

/// One census of a run at its window: the CPU rate the window was taken at (the envelope's and what was
/// charged before it), the window, and the steady CPU the census measured at it.
struct Iteration {
    rate: u64,
    window: Duration,
    measured: u64,
}

/// The run on `driver`: its census at the window the node's CPU control enforces with, the cost and evidence
/// derived from it, written under `out` as `cost.json`, `evidence` and `samples`, the cost's id the evidence's
/// SHA-256. A window that depends on the VM's rate (the Mac's cycle) is taken first at the envelope's rate,
/// then at the envelope's and what the census before measured, until a census measures no more than the
/// window was taken at: that is charged, and its window is the one the allocation's own rate is enforced over
/// (the charge only grows, so the run ends). `prefix` goes at the evidence's head as it is (a Mac's
/// CPU-response section, its rows read back by the node; empty elsewhere).
pub fn produce<D: Driver>(
    driver: &mut D,
    plan: &Plan,
    out: &Path,
    windows: &Windows,
    prefix: &[u8],
) -> Result<Made, String> {
    let class = driver.class();
    let described = driver.describe()?;
    let envelope = driver.envelope()?;
    let patience = driver.patience()?;
    let page = rustix::param::page_size();
    let block = rustix::fs::statvfs(driver.runtime())
        .map_err(|e| format!("reading {}'s filesystem: {e}", driver.runtime().display()))?
        .f_frsize;
    let quantum = CapacityResources {
        cpu_milli: 1,
        mem_bytes: u64::try_from(page).map_err(|e| e.to_string())?,
        disk_bytes: block,
    };
    std::fs::create_dir_all(out).map_err(|e| format!("making {}: {e}", out.display()))?;
    let mut record = Record::create(out.join("samples"))?;
    let mut iterations: Vec<Iteration> = Vec::new();
    let mut caches = Files::new();
    let mut charged = 0u64;
    let (window, census, mut overhead, inside, boot) = loop {
        let rate = envelope
            .resources
            .cpu_milli
            .checked_add(charged)
            .ok_or("the CPU rate overflows")?;
        let window = windows.at(rate)?;
        if window.length.is_zero() {
            return Err("a window of no time".to_owned());
        }
        record.line(&format!(
            "census {} window_ns {} rate_cpu_milli {rate}",
            iterations.len(),
            window.length.as_nanos()
        ))?;
        let census = census(
            driver,
            plan.seedable,
            patience.bound,
            &window,
            envelope.vcpus,
            iterations.is_empty(),
            &mut record,
        )?;
        caches.extend(census.caches.iter().cloned());
        let (overhead, inside, boot) = derive(envelope.resources, &census, plan.seedable)?;
        iterations.push(Iteration {
            rate,
            window: window.length,
            measured: overhead.cpu_milli,
        });
        if !windows.depends_on_rate() || overhead.cpu_milli <= charged {
            break (window, census, overhead, inside, boot);
        }
        charged = overhead.cpu_milli;
    };
    if windows.depends_on_rate() {
        overhead.cpu_milli = charged;
    }
    let (samples_file, samples_sha256) = record.finish()?;

    let mut body = String::new();
    let mut line = |text: String| {
        body.push_str(&text);
        body.push('\n');
    };
    line(HEADER.to_owned());
    line(format!(
        "class {class} on {} {}",
        std::env::consts::OS,
        std::env::consts::ARCH
    ));
    for (name, value) in described {
        line(format!("config {name} {value}"));
    }
    let resources = |r: CapacityResources| {
        format!(
            "cpu_milli={} mem_bytes={} disk_bytes={}",
            r.cpu_milli, r.mem_bytes, r.disk_bytes
        )
    };
    line(format!(
        "envelope {} vcpus={} ({}): floor and ceiling",
        resources(envelope.resources),
        envelope.vcpus,
        envelope.source
    ));
    line(format!(
        "quantum {} (cpu: the API's milli-CPU; mem: this host's page size; disk: statvfs f_frsize of {})",
        resources(quantum),
        driver.runtime().display()
    ));
    line(format!(
        "window {} ns ({})",
        window.length.as_nanos(),
        window.source
    ));
    for (k, it) in iterations.iter().enumerate() {
        line(format!(
            "window_census {k} rate_cpu_milli={} window_ns={} steady_cpu_milli={}{}",
            it.rate,
            it.window.as_nanos(),
            it.measured,
            if k + 1 == iterations.len() {
                " (charged: no more than its window's rate was taken at)"
            } else {
                " (more than its window's rate was taken at: the next census at the rate with it)"
            }
        ));
    }
    line(format!(
        "census seedable={} (SEEDABLE, bench/competitive/probes/waits_census.py, decision 9) ready={} ran_out={} first_bound_ns={} ({}), doubled at each run-out; each guest read at every window boundary from its VMM's launch through its readiness and an idle hold as long as its boot took and at least one whole window after it, to a whole window",
        plan.seedable,
        census.samples.len(),
        census.ran_out.len(),
        census.first.as_nanos(),
        patience.source
    ));
    line(format!(
        "overhead {} at RAM {} bytes (the envelope's: a cost is per envelope) (the census maximum of each ready guest's: its VMM's own CPU in one window from its readiness on, its guest's left out, rounded up; its VMM's own memory, as the memory source says; the bytes held in its own folders at a reading, there during its life and neither before it nor after it) (evidence alone, not in the cost: a node learns a VM's overhead from its windows, decision 9)",
        resources(overhead),
        envelope.resources.mem_bytes
    ));
    line(format!(
        "boot_burst cpu_milli={boot} (the census maximum of its VMM's own CPU in one window that began before its guest was ready: not charged; for concurrent-boot admission)"
    ));
    line(format!(
        "file_pages bytes={} (the census maximum of the page cache its OS charged its VMM apart: the images it read, shared by every VM that reads them and reclaimable: not charged)",
        census
            .samples
            .iter()
            .map(|sample| sample.file_bytes)
            .max()
            .unwrap_or(0)
    ));
    for (name, value) in driver.sources() {
        line(format!("source {name} {value}"));
    }
    if caches.is_empty() {
        line("disk_host none (the first guest left nothing new on disk)".to_owned());
    }
    for held in &caches {
        line(format!(
            "disk_host {} {} (left by the first guest and found by every later one, which left nothing new: a one-time cache of the host's, no one VM's, left out)",
            held.path.display(),
            held.bytes
        ));
    }
    line(format!(
        "guest_overhead {} (the census maximum of the envelope less what the guest's agent reported free) (evidence alone, not in the cost: a node learns a guest's own from its backing's windows, decision 9)",
        resources(inside)
    ));
    line(format!(
        "samples {} sha256={} (each census's ready guests' readings)",
        samples_file.display(),
        hex(&samples_sha256)
    ));
    line("guests ready_ns windows longest_ns cpu_milli boot_milli total_milli holdings mem_bytes file_bytes behind disk_bytes free_cpu_milli free_mem_bytes free_disk_bytes (the charged census's; holdings: readings of what it held; behind: those whose footprint was below their guest RAM's pages)".to_owned());
    for (n, s) in census.samples.iter().enumerate() {
        line(format!(
            "guest {n} {} {} {} {} {} {} {} {} {} {} {} {} {} {}",
            s.ready.as_nanos(),
            s.windows,
            s.longest.as_nanos(),
            s.cpu_milli,
            s.boot_milli,
            s.total_milli,
            s.holdings,
            s.mem_bytes,
            s.file_bytes,
            s.behind,
            s.disk_bytes,
            s.free.cpu_milli,
            s.free.mem_bytes,
            s.free.disk_bytes
        ));
    }
    for (n, bound) in census.ran_out.iter().enumerate() {
        line(format!("ran_out {n} {}", bound.as_nanos()));
    }
    let mut evidence = Vec::with_capacity(prefix.len().saturating_add(body.len()));
    evidence.extend_from_slice(prefix);
    evidence.extend_from_slice(body.as_bytes());
    // The measured overhead and guest allowance are evidence alone, never a number a node starts from (joint plan step
    // 14; ROOT, 2026-11-13, by decision 9's rule): the cost is the envelope alone, and its id names the evidence.
    let cost = RuntimeCost {
        id: Sha256::digest(&evidence).into(),
        floor: envelope.resources,
        ceiling: envelope.resources,
        quantum,
        guest_overhead: CapacityResources::default(),
        overhead: CapacityResources::default(),
    };
    cost.check().map_err(|e| e.to_string())?;
    cost.allocate(CapacityResources::default())
        .map_err(|e| format!("the derived cost admits no guest: {e}"))?;
    let cost_bytes = serde_json::to_vec(&cost).map_err(|e| e.to_string())?;
    let made = Made {
        summary: String::new(),
        cost_file: out.join("cost.json"),
        cost_sha256: Sha256::digest(&cost_bytes).into(),
        evidence_file: out.join("evidence"),
        samples_file,
    };
    std::fs::write(&made.evidence_file, &evidence).map_err(|e| e.to_string())?;
    std::fs::write(&made.cost_file, &cost_bytes).map_err(|e| e.to_string())?;
    let mut summary = String::new();
    let _ = write!(
        summary,
        "{class}: {} guests ready, {} ran out, window {:?} after {} census(es); overhead {}; boot burst cpu_milli={boot}; guest_overhead {}",
        census.samples.len(),
        census.ran_out.len(),
        window.length,
        iterations.len(),
        resources(overhead),
        resources(inside)
    );
    Ok(Made { summary, ..made })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::seam::{Envelope, Launched, Lines, Patience};

    /// One scripted guest: its readings in turn (the last repeated), what its agent reports, and what it
    /// leaves on disk after its end.
    #[derive(Clone)]
    struct Script {
        uses: Vec<Use>,
        holdings: Vec<Holding>,
        free: CapacityResources,
        left: Files,
    }

    /// Readings in turn, the last repeated.
    struct Scripted<R>(Vec<R>, usize);

    impl<R: Clone + Send + 'static> Probe for Scripted<R> {
        type Reading = R;
        fn read(&mut self) -> Result<R, String> {
            let reading = self
                .0
                .get(self.1)
                .or(self.0.last())
                .cloned()
                .ok_or("no reading scripted")?;
            self.1 += 1;
            Ok(reading)
        }
    }

    /// A scripted class: each launch is ready at once or runs out, its readings and its agent's report as
    /// scripted, its folders holding what the guests before it left. No VM runs.
    struct Driven {
        runtime: PathBuf,
        boots: Vec<Option<Script>>,
        launched: usize,
        torn_down: usize,
        bounds: Vec<Duration>,
        disk: Files,
    }

    impl Driven {
        fn new(runtime: &Path, boots: Vec<Option<Script>>) -> Self {
            Self {
                runtime: runtime.to_owned(),
                boots,
                launched: 0,
                torn_down: 0,
                bounds: Vec::new(),
                disk: Vec::new(),
            }
        }
    }

    impl Driver for Driven {
        type Guest = Option<Script>;
        type Cpu = Scripted<Use>;
        type Holds = Scripted<Holding>;
        fn class(&self) -> String {
            "scripted".to_owned()
        }
        fn describe(&self) -> Result<Lines, String> {
            Ok(vec![("image".to_owned(), "none".to_owned())])
        }
        fn envelope(&self) -> Result<Envelope, String> {
            Ok(Envelope {
                resources: envelope(),
                vcpus: 2,
                source: "scripted".to_owned(),
            })
        }
        fn patience(&self) -> Result<Patience, String> {
            Ok(Patience {
                bound: Duration::from_secs(1),
                source: "scripted".to_owned(),
            })
        }
        fn runtime(&self) -> &Path {
            &self.runtime
        }
        fn sources(&self) -> Lines {
            vec![("cpu".to_owned(), "scripted".to_owned())]
        }
        fn held(&self) -> Result<Files, String> {
            Ok(self.disk.clone())
        }
        fn launch(&mut self, _: Instant) -> Result<Launched<Self>, String> {
            let next = self
                .boots
                .get(self.launched)
                .cloned()
                .ok_or("no launch left")?;
            self.launched += 1;
            let (uses, holdings) = next
                .as_ref()
                .map(|s| (s.uses.clone(), s.holdings.clone()))
                .unwrap_or_default();
            Ok((next, Scripted(uses, 0), Scripted(holdings, 0)))
        }
        fn ready(&mut self, guest: &mut Option<Script>, deadline: Instant) -> Result<bool, String> {
            self.bounds
                .push(deadline.saturating_duration_since(Instant::now()));
            Ok(guest.is_some())
        }
        fn idle(&mut self, _: &Option<Script>, until: Instant) -> Result<(), String> {
            while let Some(left) = until.checked_duration_since(Instant::now()) {
                std::thread::park_timeout(left);
            }
            Ok(())
        }
        fn report(
            &mut self,
            guest: &Option<Script>,
            _: Instant,
        ) -> Result<CapacityResources, String> {
            guest
                .as_ref()
                .map(|s| s.free)
                .ok_or("no guest to report".to_owned())
        }
        fn tear_down(&mut self, guest: Option<Script>, _: Instant) -> Result<(), String> {
            self.torn_down += 1;
            if let Some(script) = guest {
                self.disk.extend(script.left);
                self.disk.sort();
            }
            Ok(())
        }
    }

    fn envelope() -> CapacityResources {
        CapacityResources {
            cpu_milli: 2000,
            mem_bytes: 512 << 20,
            disk_bytes: 0,
        }
    }

    fn free(mem: u64) -> CapacityResources {
        CapacityResources {
            cpu_milli: 2000,
            mem_bytes: mem,
            disk_bytes: 64 << 20,
        }
    }

    /// A VMM that used `cpu_ms` of CPU of which `guest_ms` its guest's, with `own` bytes of its own memory
    /// and `guest` of its guest's RAM, holding `files`.
    fn reading(cpu_ms: u64, guest_ms: u64, own: u64, guest: u64, files: Files) -> (Use, Holding) {
        (
            Use {
                cpu: Duration::from_millis(cpu_ms),
                guest_cpu: Duration::from_millis(guest_ms),
                guest_threads: None,
            },
            Holding {
                footprint: own + guest,
                file_pages: None,
                peak_footprint: None,
                guest_memory: Some(guest),
                files,
            },
        )
    }

    fn script(readings: Vec<(Use, Holding)>, free_mem: u64) -> Script {
        let (uses, holdings) = readings.into_iter().unzip();
        Script {
            uses,
            holdings,
            free: free(free_mem),
            left: Vec::new(),
        }
    }

    /// Both readings of each tick, as one probe would have made them.
    fn readings(ticks: Vec<(u64, (Use, Holding))>) -> Readings {
        let (cpu, holdings) = ticks
            .into_iter()
            .map(|(ms, (cpu, held))| (tick(ms, cpu), tick(ms, held)))
            .unzip();
        Readings { cpu, holdings }
    }

    fn window(ms: u64) -> Window {
        Window {
            length: Duration::from_millis(ms),
            source: "scripted".to_owned(),
        }
    }

    fn record(dir: &Path) -> Record {
        Record::create(dir.join("samples")).unwrap()
    }

    fn tick<R>(ms: u64, reading: R) -> Tick<R> {
        Tick {
            at: Duration::from_millis(ms),
            reading,
        }
    }

    /// AC-T7.4.1: what folders hold: expect each path under them with its allocated bytes, a file linked
    /// twice counted once, sorted by path, and a folder that isn't there passed over.
    #[test]
    fn the_walk_lists_each_path_and_its_allocated_bytes() {
        let dir = tempfile::tempdir().unwrap();
        let inner = dir.path().join("inner");
        std::fs::create_dir(&inner).unwrap();
        std::fs::write(inner.join("file"), vec![7u8; 64 * 1024]).unwrap();
        std::fs::hard_link(inner.join("file"), dir.path().join("link")).unwrap();
        let held = walk(&[dir.path().to_owned(), dir.path().join("absent")]).unwrap();
        let mut sorted = held.clone();
        sorted.sort();
        assert_eq!(held, sorted);
        let files: Vec<&Held> = held.iter().filter(|h| h.bytes >= 64 * 1024).collect();
        assert_eq!(files.len(), 1, "{held:?}");
        assert!(held.iter().any(|h| h.path == inner));
        assert!(!held.iter().any(|h| h.path.ends_with("absent")));
    }

    /// AC-T7.4.1: the plan's SEEDABLE is the census's own.
    #[test]
    fn the_plan_takes_the_census_rules_seedable() {
        assert_eq!(Plan::derived().unwrap().seedable, 1_000);
    }

    /// AC-T7.4.1 (ROOT: the steady state is charged, the boot's burst recorded): a guest ready at 150 ms whose
    /// readings each charge their window: expect each window's own CPU (its VMM's less its guest's) over the
    /// window, the first from the launch; a reading late after a stall charged over its whole span, an early
    /// one over no less than the window; own CPU that reads lower than before (its counters' ticks) charged
    /// nothing and its dip never charged again; the windows that began before 150 ms its boot's burst, the
    /// rest its charged steady state; and its memory its footprint less its guest's RAM at a reading from its
    /// readiness on (no enforced peak: the Mac's rule).
    #[test]
    fn each_window_charges_its_own_cpu_and_the_most_own_memory() {
        let ticks = readings(vec![
            (100, reading(50, 30, 30 << 20, 1 << 20, Vec::new())),
            (190, reading(70, 45, 10 << 20, 2 << 20, Vec::new())),
            (400, reading(80, 50, 20 << 20, 8 << 20, Vec::new())),
            (500, reading(85, 60, 20 << 20, 8 << 20, Vec::new())),
            (600, reading(100, 60, 20 << 20, 8 << 20, Vec::new())),
        ]);
        let sample = measured(
            Duration::from_millis(150),
            &ticks,
            free(0),
            Duration::from_millis(100),
            2,
            &Vec::new(),
            &Vec::new(),
        )
        .unwrap();
        // Boot, the windows from 0 and from 100 ms: 20 ms of its own in the first 100 ms (200 milli); 5 ms
        // in 90 ms, charged over 100 (50).
        assert_eq!(sample.boot_milli, 200);
        // Steady, the windows from 190 ms on: 5 ms in a late 210 ms (24, rounded up); none where it reads
        // 25 against 30 before; then 10 ms above the 30 it had reached (100), never 15 above its dip (150).
        assert_eq!(sample.cpu_milli, 100);
        // The whole VMM's: 50 ms in the first 100 ms.
        assert_eq!(sample.total_milli, 500);
        // From 150 ms on: 20 MiB its own; the 30 MiB of its boot's first reading not charged.
        assert_eq!(sample.mem_bytes, 20 << 20);
        assert_eq!(sample.behind, 0);
        assert_eq!(sample.longest, Duration::from_millis(210));
        assert_eq!((sample.windows, sample.holdings), (5, 5));
    }

    /// AC-T7.4.1 (ROOT: the page cache is no one VM's cost): readings whose OS charges the VMM page cache apart,
    /// 20 MiB of it at the first: expect the VMM's own memory its footprint less its guest's pages, the page
    /// cache recorded at its most and not charged.
    #[test]
    fn page_cache_is_recorded_and_not_charged() {
        let cached = |file: u64| {
            let (used, mut held) = reading(1, 0, 8 << 20, 160 << 20, Vec::new());
            held.file_pages = Some(file);
            (used, held)
        };
        let sample = measured(
            Duration::ZERO,
            &readings(vec![(100, cached(20 << 20)), (200, cached(0))]),
            free(0),
            Duration::from_millis(100),
            2,
            &Vec::new(),
            &Vec::new(),
        )
        .unwrap();
        assert_eq!((sample.mem_bytes, sample.file_bytes), (8 << 20, 20 << 20));
    }

    /// AC-T7.4.1: a guest whose last reading never told its RAM apart, or whose VMM names other than its
    /// vCPUs' threads, is refused, never measured; a reading whose footprint is below its guest's RAM (a
    /// ledger behind its pages) counts none of the VMM's own and is counted.
    #[test]
    fn a_reading_that_cannot_tell_the_guest_apart_is_refused() {
        let measure = |r: (Use, Holding)| {
            measured(
                Duration::ZERO,
                &readings(vec![(100, r)]),
                free(0),
                Duration::from_millis(100),
                2,
                &Vec::new(),
                &Vec::new(),
            )
        };
        let mut unknown = reading(1, 0, 1, 1, Vec::new());
        unknown.1.guest_memory = None;
        assert!(measure(unknown).is_err());
        let mut behind = reading(1, 0, 1, 1, Vec::new());
        behind.1.footprint = 0;
        let sample = measure(behind).unwrap();
        assert_eq!((sample.behind, sample.mem_bytes), (1, 0));
        let mut threads = reading(1, 0, 1, 1, Vec::new());
        threads.0.guest_threads = Some(1);
        assert!(measure(threads.clone()).is_err());
        threads.0.guest_threads = Some(2);
        assert!(measure(threads).is_ok());
    }

    /// AC-T7.4.1 (ROOT, CPU per window): a guest ready at once whose VMM used 1 ms of its own (40 ms in all,
    /// 39 its guest's) before its first boundary: expect that millisecond its boot's burst over the window,
    /// never over its boot's microseconds (the old rate over a boot-sized hold), its guest's time left out,
    /// and nothing charged, its steady state having used none.
    #[test]
    fn a_short_boot_does_not_set_the_cpu_figure_from_noise() {
        let dir = tempfile::tempdir().unwrap();
        let mut driver = Driven::new(
            dir.path(),
            vec![
                Some(script(
                    vec![reading(40, 39, 1 << 20, 1 << 20, Vec::new())],
                    400 << 20
                ));
                2
            ],
        );
        let window = window(50);
        let census = census(
            &mut driver,
            2,
            Duration::from_secs(1),
            &window,
            2,
            true,
            &mut record(dir.path()),
        )
        .unwrap();
        for sample in &census.samples {
            assert!(sample.ready < window.length, "{sample:?}");
            assert!(
                (1..=20).contains(&sample.boot_milli),
                "1 ms over a 50 ms window: {sample:?}"
            );
            assert!(sample.total_milli >= 10 * sample.boot_milli, "{sample:?}");
        }
        let (overhead, _, boot) = derive(envelope(), &census, 2).unwrap();
        assert_eq!(overhead.cpu_milli, 0);
        assert!((1..=20).contains(&boot), "{boot}");
    }

    /// AC-T7.4.1 (ROOT, VMM memory): a guest that touched 30 MiB of its 512 MiB, its VMM holding 60 MiB of
    /// its own: expect the VMM's 60 MiB as the memory overhead, not nothing because its footprint stayed
    /// under the envelope's.
    #[test]
    fn a_partial_touch_guest_still_shows_vmm_memory() {
        let dir = tempfile::tempdir().unwrap();
        let mut driver = Driven::new(
            dir.path(),
            vec![
                Some(script(
                    vec![reading(1, 0, 60 << 20, 30 << 20, Vec::new())],
                    400 << 20
                ));
                2
            ],
        );
        let window = window(5);
        let census = census(
            &mut driver,
            2,
            Duration::from_secs(1),
            &window,
            2,
            true,
            &mut record(dir.path()),
        )
        .unwrap();
        let (overhead, inside, _) = derive(envelope(), &census, 2).unwrap();
        assert_eq!(overhead.mem_bytes, 60 << 20);
        assert_eq!(inside.mem_bytes, (512 << 20) - (400 << 20));
    }

    /// AC-T7.4.1 (ROOT, disk per VM): the first guest writes the host's carried VMM binary into the cache,
    /// under a name of its own first and then renamed, and keeps its socket's folder while it runs; the
    /// second finds the binary there: expect the disk overhead the socket folder's bytes alone, the binary
    /// named as the host's cache, and a later guest that leaves something new refused.
    #[test]
    fn a_first_boot_cache_write_is_not_disk_overhead() {
        let dir = tempfile::tempdir().unwrap();
        let held = |path: &str, file: u64, bytes: u64| Held {
            path: PathBuf::from(path),
            file: (1, file),
            bytes,
        };
        let writing = held("/runtime/cache/.shards-vm.part", 7, 2 << 20);
        let binary = held("/runtime/cache/shards-vm", 7, 2 << 20);
        let socket = held("/tmp/guest/v", 9, 4096);
        let mut first = script(
            vec![
                reading(1, 0, 1, 1, vec![writing, socket.clone()]),
                reading(1, 0, 1, 1, vec![binary.clone(), socket.clone()]),
            ],
            400 << 20,
        );
        first.left = vec![binary.clone()];
        let later = script(
            vec![reading(1, 0, 1, 1, vec![binary.clone(), socket])],
            400 << 20,
        );
        let mut driver = Driven::new(dir.path(), vec![Some(first.clone()), Some(later.clone())]);
        let window = window(5);
        let taken = census(
            &mut driver,
            2,
            Duration::from_secs(1),
            &window,
            2,
            true,
            &mut record(dir.path()),
        )
        .unwrap();
        let (overhead, _, _) = derive(envelope(), &taken, 2).unwrap();
        assert_eq!(overhead.disk_bytes, 4096);
        assert_eq!(taken.caches, vec![binary]);

        let mut leaving = later;
        leaving.left = vec![held("/runtime/home/guest-1.log", 11, 8192)];
        let mut driver = Driven::new(dir.path(), vec![Some(first), Some(leaving)]);
        let refused = census(
            &mut driver,
            2,
            Duration::from_secs(1),
            &window,
            2,
            true,
            &mut record(dir.path()),
        );
        assert!(refused.is_err());
        assert_eq!(driver.torn_down, 2);
    }

    /// AC-T7.4.1: a census of three ready guests and two run-outs between them: expect each run-out's
    /// bound recorded, the bound doubled after each, and every launched guest torn down.
    #[test]
    fn the_census_doubles_at_run_outs_and_tears_every_guest_down() {
        let dir = tempfile::tempdir().unwrap();
        let ready = Some(script(vec![reading(1, 0, 1, 1, Vec::new())], 400 << 20));
        let mut driver = Driven::new(
            dir.path(),
            vec![ready.clone(), None, ready.clone(), None, ready],
        );
        let window = window(5);
        let census = census(
            &mut driver,
            3,
            Duration::from_secs(1),
            &window,
            2,
            true,
            &mut record(dir.path()),
        )
        .unwrap();
        assert_eq!(census.samples.len(), 3);
        assert_eq!(
            census.ran_out,
            vec![Duration::from_secs(1), Duration::from_secs(2)]
        );
        assert!(
            driver.bounds[4] > Duration::from_millis(3_900),
            "{:?}",
            driver.bounds
        );
        assert_eq!(driver.torn_down, 5);
        assert!(derive(envelope(), &census, 4).is_err());
    }

    /// AC-T7.4.1: a class whose guests run out as often as the census needs answers is refused, not
    /// waited on forever.
    #[test]
    fn as_many_run_outs_as_samples_refuse_the_run() {
        let dir = tempfile::tempdir().unwrap();
        let mut driver = Driven::new(dir.path(), vec![None, None, None]);
        let window = window(5);
        assert!(
            census(
                &mut driver,
                2,
                Duration::from_secs(1),
                &window,
                2,
                true,
                &mut record(dir.path())
            )
            .is_err()
        );
        assert_eq!(driver.launched, 2);
    }

    /// AC-T7.4.1 (ROOT, the Mac window's fixed point): a window that the rate sets, over guests whose VMM uses
    /// 1 ms of its own in each window of their steady state: expect the first census at the envelope's rate,
    /// each next at the envelope's and what the one before measured, the run ending at the first census that
    /// measures no more than its window's rate was taken at, and each census in the evidence. What the last
    /// measured is evidence alone (joint plan step 14; ROOT, 2026-11-13, by decision 9's rule): the cost written
    /// carries no overhead and no guest allowance, and its evidence names the measured overhead.
    #[test]
    fn the_mac_window_settles_at_the_rate_it_charges() {
        let dir = tempfile::tempdir().unwrap();
        // Its own CPU grows 1 ms a reading, its guest's not at all.
        let steady: Vec<(Use, Holding)> = (0..64)
            .map(|n| reading(n, 0, 1 << 20, 1 << 20, Vec::new()))
            .collect();
        let mut driver = Driven::new(dir.path(), vec![Some(script(steady, 400 << 20)); 64]);
        let windows = Windows::AtRate(Box::new(|rate| {
            Ok(Window {
                length: Duration::from_millis(if rate == 2000 { 40 } else { 20 }),
                source: format!("at {rate}"),
            })
        }));
        let out = dir.path().join("out");
        let made = produce(&mut driver, &Plan { seedable: 2 }, &out, &windows, &[]).unwrap();
        let text = std::fs::read_to_string(&made.evidence_file).unwrap();
        let censuses: Vec<(u64, u64)> = text
            .lines()
            .filter_map(|l| l.strip_prefix("window_census "))
            .map(|l| {
                let field = |name: &str| -> u64 {
                    l.split(' ')
                        .find_map(|w| w.strip_prefix(name))
                        .unwrap()
                        .parse()
                        .unwrap()
                };
                (field("rate_cpu_milli="), field("steady_cpu_milli="))
            })
            .collect();
        assert!(censuses.len() >= 2, "{text}");
        assert_eq!(censuses[0].0, 2000);
        for pair in censuses.windows(2) {
            assert_eq!(pair[1].0, 2000 + pair[0].1, "{censuses:?}");
            assert!(pair[0].1 > pair[0].0 - 2000, "{censuses:?}");
        }
        let (rate, measured) = *censuses.last().unwrap();
        assert!(measured <= rate - 2000, "{censuses:?}");
        let written: RuntimeCost =
            serde_json::from_slice(&std::fs::read(&made.cost_file).unwrap()).unwrap();
        assert_eq!(written.overhead, CapacityResources::default());
        assert_eq!(written.guest_overhead, CapacityResources::default());
        assert!(
            text.contains(&format!("\noverhead cpu_milli={} ", rate - 2000)),
            "{text}"
        );
        assert!(text.contains("\nwindow 20000000 ns (at "), "{text}");
    }

    /// AC-T7.4.1: produce over a scripted class, with and without a CPU-response section at its head: expect
    /// the cost and evidence written, the cost's id the evidence's SHA-256, the pair loaded as a node loads
    /// it with the section's rows, and the evidence naming its window and its samples by their SHA-256.
    #[test]
    fn the_pair_loads_as_the_node_loads_it() {
        let row = hex_runtime::guest::CpuResponse {
            host: "Mac17,6".to_owned(),
            samples: 3,
            p50: Duration::from_nanos(5),
            p90: Duration::from_nanos(7),
            p99: Duration::from_nanos(11),
            max: Duration::from_nanos(13),
            raw: [0xab; 32],
        };
        let section = hex_runtime::guest::cpu_response_section(&[row.clone()]).unwrap();
        for (prefix, rows) in [(Vec::new(), Vec::new()), (section, vec![row])] {
            let dir = tempfile::tempdir().unwrap();
            let mut driver = Driven::new(
                dir.path(),
                vec![Some(script(vec![reading(1, 0, 1, 1, Vec::new())], 400 << 20)); 2],
            );
            let out = dir.path().join("out");
            let made = produce(
                &mut driver,
                &Plan { seedable: 2 },
                &out,
                &Windows::Fixed(window(5)),
                &prefix,
            )
            .unwrap();
            let (cost, _, read) = hex_runtime::guest::load_runtime_evidence(
                &made.cost_file,
                made.cost_sha256,
                &made.evidence_file,
            )
            .unwrap();
            assert_eq!(read, rows);
            let evidence = std::fs::read(&made.evidence_file).unwrap();
            let digest: [u8; 32] = Sha256::digest(&evidence).into();
            assert_eq!(cost.id, digest);
            assert!(evidence.starts_with(&prefix));
            let text = String::from_utf8(evidence[prefix.len()..].to_vec()).unwrap();
            assert!(text.starts_with("hex-guest-cost 2\n"));
            assert_eq!((cost.floor, cost.ceiling), (envelope(), envelope()));
            assert_eq!(cost.quantum.cpu_milli, 1);
            assert!(text.contains("\nwindow 5000000 ns (scripted)\n"), "{text}");
            let samples = std::fs::read(&made.samples_file).unwrap();
            let named = format!(
                "samples {} sha256={}",
                made.samples_file.display(),
                hex(&Sha256::digest(&samples))
            );
            assert!(text.contains(&named), "{text}");
            assert_eq!(text.lines().filter(|l| l.starts_with("guest ")).count(), 2);
        }
    }
}
