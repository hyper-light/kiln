//! The node's own disk probe (`AC-T7.10.3`; `docs/design/failure-isolation.md` F3): a thread writes
//! and flushes a small record on the node's state disk every period, and the node judges the
//! outstanding write's age with [`hex_health::probe::DiskProbe`]. A write that blocks blocks only the
//! prober's thread: the judge reads when it began, so a stalled disk is seen while it stalls, not when
//! the write at last returns. At `Drain` the node cordons itself: it takes no new work and its work is
//! moved.
//!
//! Bounds (CLAUDE.md §2): one write outstanding at a time, one fixed-size record, one thread.

use std::fs::{File, OpenOptions};
use std::io::{Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex, MutexGuard};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use hex_health::probe::DiskProbe;
use hex_health::{HealthError, Level};

/// The record written each period: one byte, the least write that dirties a block, so the flush
/// must write that block to the device (fdatasync(2)); nothing about the disk is assumed.
pub const RECORD_BYTES: usize = 1;

/// macOS's recorded probe answers, `(latency ns, count)`, by the probe's histogram buckets (each the
/// bucket's highest value, the slowest exact): `bench/health/records/2026-11-06-probe-floor-apfs.md`,
/// its third run, APFS on an M4 Max under load, 23,764 probes, the slowest 586.1 ms.
const MACOS_RECORD: [(u64, u32); 31] = [
    (2_621_439, 9),
    (3_145_727, 51),
    (3_670_015, 1_450),
    (4_194_303, 3_996),
    (5_242_879, 3_571),
    (6_291_455, 819),
    (7_340_031, 362),
    (8_388_607, 1_792),
    (10_485_759, 2_844),
    (12_582_911, 2_394),
    (14_680_063, 1_953),
    (16_777_215, 1_507),
    (20_971_519, 1_640),
    (25_165_823, 695),
    (29_360_127, 330),
    (33_554_431, 136),
    (41_943_039, 119),
    (50_331_647, 42),
    (58_720_255, 19),
    (67_108_863, 7),
    (83_886_079, 10),
    (100_663_295, 5),
    (117_440_511, 4),
    (134_217_727, 2),
    (167_772_159, 1),
    (201_326_591, 1),
    (234_881_023, 1),
    (268_435_455, 1),
    (335_544_319, 1),
    (536_870_911, 1),
    (586_085_750, 1),
];

/// This host class's recorded healthy probe answers: the seed of the probe's judgement. A host class
/// with no record is refused, typed: its distribution is recorded first (`examples/fsync_latency`).
pub fn recorded() -> Result<&'static [(u64, u32)], ProbeError> {
    if cfg!(target_os = "macos") {
        Ok(&MACOS_RECORD)
    } else {
        Err(ProbeError::NoRecord(std::env::consts::OS))
    }
}

/// Every way the probe refuses, typed (CLAUDE.md §3).
#[derive(Debug, thiserror::Error)]
pub enum ProbeError {
    #[error("the disk probe's record {path}: {source}")]
    Record {
        path: PathBuf,
        source: std::io::Error,
    },
    #[error("the disk probe's write and flush: {0}")]
    Write(std::io::Error),
    #[error(transparent)]
    Health(#[from] HealthError),
    #[error("the disk probe's state lock was poisoned by a thread that panicked")]
    Poisoned,
    #[error("the disk probe's thread could not start: {0}")]
    Spawn(std::io::Error),
    #[error("the disk probe's thread ended: {0}")]
    Ended(String),
    #[error("the clock passed u64 nanoseconds")]
    Clock,
    #[error("no recorded disk-probe distribution for {0}: record one first")]
    NoRecord(&'static str),
}

/// What the prober and the judge share.
#[derive(Debug)]
struct Shared {
    probe: Mutex<DiskProbe>,
    /// Set once the judge saw `Drain`; never cleared (a node is uncordoned by a person or the control
    /// plane, not by the disk answering again).
    cordoned: AtomicBool,
    /// Set to stop the prober; `wake` tells it at once rather than at the end of its period.
    stop: Mutex<bool>,
    wake: Condvar,
    /// The prober's last write error, if it ended on one.
    failed: Mutex<Option<String>>,
}

/// A running probe: its thread, and the judge the node agent asks.
#[derive(Debug)]
pub struct DiskProber {
    shared: Arc<Shared>,
    epoch: Instant,
    thread: Option<JoinHandle<()>>,
}

fn lock<T>(m: &Mutex<T>) -> Result<MutexGuard<'_, T>, ProbeError> {
    m.lock().map_err(|_| ProbeError::Poisoned)
}

fn since(epoch: Instant) -> Result<u64, ProbeError> {
    u64::try_from(epoch.elapsed().as_nanos()).map_err(|_| ProbeError::Clock)
}

/// One probe: rewrite the record in place and flush it to the disk.
fn write_record(file: &mut File, record: &[u8]) -> std::io::Result<()> {
    file.seek(SeekFrom::Start(0))?;
    file.write_all(record)?;
    file.sync_data()
}

impl DiskProber {
    /// Start probing the disk under `state_dir` every `period`, its record at `state_dir/.hex-probe`,
    /// judged from its host class's `recorded` answers and the measured time to `replace` one of the
    /// node's workloads elsewhere (what a false drain costs).
    pub fn start(
        state_dir: &Path,
        period: Duration,
        recorded: &[(u64, u32)],
        replace: Duration,
    ) -> Result<Self, ProbeError> {
        let replace_ns = u64::try_from(replace.as_nanos()).map_err(|_| ProbeError::Clock)?;
        let probe = DiskProbe::new(recorded, replace_ns)?;
        let path = state_dir.join(".hex-probe");
        let mut file = OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .open(&path)
            .map_err(|source| ProbeError::Record {
                path: path.clone(),
                source,
            })?;
        let shared = Arc::new(Shared {
            probe: Mutex::new(probe),
            cordoned: AtomicBool::new(false),
            stop: Mutex::new(false),
            wake: Condvar::new(),
            failed: Mutex::new(None),
        });
        let epoch = Instant::now();
        let ours = Arc::clone(&shared);
        let thread = std::thread::Builder::new()
            .name("hex-disk-probe".into())
            .spawn(move || {
                let record = [0u8; RECORD_BYTES];
                if let Err(e) =
                    probe_loop(&ours, epoch, period, || write_record(&mut file, &record))
                    && let Ok(mut f) = ours.failed.lock()
                {
                    *f = Some(e.to_string());
                }
            })
            .map_err(ProbeError::Spawn)?;
        Ok(Self {
            shared,
            epoch,
            thread: Some(thread),
        })
    }

    /// Judge the disk now: the outstanding write's age and the completed probes' latencies, and cordon
    /// the node at `Drain`. Refuses once the prober's thread ended on an error.
    pub fn level(&self) -> Result<Level, ProbeError> {
        if let Some(e) = lock(&self.shared.failed)?.as_ref() {
            return Err(ProbeError::Ended(e.clone()));
        }
        // The clock is read under the lock: read before it, the prober could finish a probe stamped
        // later in between, and the judgement's time would run backwards.
        let probe = lock(&self.shared.probe)?;
        let level = probe.level(since(self.epoch)?)?;
        drop(probe);
        if level == Level::Drain {
            self.shared.cordoned.store(true, Ordering::Release);
        }
        Ok(level)
    }

    /// Whether the node may take new work: false once a judgement saw `Drain`.
    pub fn schedulable(&self) -> bool {
        !self.shared.cordoned.load(Ordering::Acquire)
    }

    /// The probe's current drain age, past which an outstanding write drains the node, for a record
    /// of the drill.
    pub fn drain_age(&self) -> Result<Duration, ProbeError> {
        Ok(Duration::from_nanos(
            lock(&self.shared.probe)?.drain_age_ns()?,
        ))
    }

    /// Stop the prober after its current write.
    fn stop(&self) {
        if let Ok(mut stop) = self.shared.stop.lock() {
            *stop = true;
        }
        self.shared.wake.notify_all();
    }
}

impl Drop for DiskProber {
    /// The prober stops after its current write. A write stalled on the disk holds its thread until
    /// the disk answers, so the thread is left to end on its own rather than joined here.
    fn drop(&mut self) {
        self.stop();
        drop(self.thread.take());
    }
}

/// The prober: start a probe, write, finish it, wait out the period; until asked to stop.
fn probe_loop(
    shared: &Shared,
    epoch: Instant,
    period: Duration,
    mut write: impl FnMut() -> std::io::Result<()>,
) -> Result<(), ProbeError> {
    loop {
        let began = Instant::now();
        {
            let mut probe = lock(&shared.probe)?;
            probe.start(since(epoch)?)?;
        }
        write().map_err(ProbeError::Write)?;
        {
            let mut probe = lock(&shared.probe)?;
            probe.finish(since(epoch)?)?;
        }
        let stop = lock(&shared.stop)?;
        let (stop, _) = shared
            .wake
            .wait_timeout_while(stop, period.saturating_sub(began.elapsed()), |s| !*s)
            .map_err(|_| ProbeError::Poisoned)?;
        if *stop {
            return Ok(());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::mpsc;

    /// The macOS/APFS record, the seed every test runs on, whatever its host (the mechanics under
    /// test don't depend on the disk), and Shards' measured boot p99 as the re-placement time.
    const RECORD: &[(u64, u32)] = &MACOS_RECORD;
    const REPLACE: Duration = Duration::from_millis(194);

    /// Wait `d` between judgements: on a channel nothing sends on, as `thread::sleep` is disallowed.
    fn pause(d: Duration) {
        let (_keep, rx) = mpsc::channel::<()>();
        let _ = rx.recv_timeout(d);
    }

    fn shared() -> Arc<Shared> {
        Arc::new(Shared {
            probe: Mutex::new(DiskProbe::new(RECORD, ns(REPLACE)).unwrap()),
            cordoned: AtomicBool::new(false),
            stop: Mutex::new(false),
            wake: Condvar::new(),
            failed: Mutex::new(None),
        })
    }

    fn ns(d: Duration) -> u64 {
        u64::try_from(d.as_nanos()).unwrap()
    }

    /// Run the prober over `write`, stalling its write number `stall_at` (0 the first) until released;
    /// returns the judge, the release and the thread.
    fn stalling(
        stall_at: u32,
    ) -> (
        DiskProber,
        mpsc::Sender<()>,
        JoinHandle<Result<(), ProbeError>>,
    ) {
        let s = shared();
        let epoch = Instant::now();
        let (release, held) = mpsc::channel::<()>();
        let ours = Arc::clone(&s);
        let thread = std::thread::spawn(move || {
            let mut n = 0u32;
            probe_loop(&ours, epoch, Duration::from_millis(5), || {
                if n == stall_at {
                    held.recv().ok();
                }
                n += 1;
                Ok(())
            })
        });
        let prober = DiskProber {
            shared: s,
            epoch,
            thread: None,
        };
        (prober, release, thread)
    }

    /// Wait until `prober` judges `want`, failing past `within`.
    fn wait_for(prober: &DiskProber, want: Level, within: Duration) {
        let end = Instant::now() + within;
        loop {
            let level = prober.level().unwrap();
            if level == want {
                return;
            }
            assert!(Instant::now() < end, "never reached {want:?}, at {level:?}");
            pause(Duration::from_millis(5));
        }
    }

    /// AC-T7.10.3: do a probe whose writes answer, then stall one (it blocks until released); expect
    /// `Drain` while it is still blocked, and the node cordoned, and cordoned still once the disk
    /// answers again. (`Suspect` comes first only where the slowest answer is under the drain age:
    /// this early, a stall per probe is likely enough that the cost rule drains inside the record's
    /// thin tail.)
    #[test]
    fn a_stalled_write_is_judged_while_it_blocks_and_cordons_the_node() {
        let (prober, release, thread) = stalling(20);
        assert!(prober.schedulable());
        wait_for(&prober, Level::Drain, Duration::from_secs(10));
        assert!(!prober.schedulable());
        prober.stop();
        release.send(()).unwrap();
        thread.join().unwrap().unwrap();
        assert!(!prober.schedulable());
    }

    /// AC-T7.10.3, before any sample: do stall the very first write; expect it judged by the record
    /// alone (no calibration, no unbounded first wait): drained past the record's drain age, the node
    /// cordoned.
    #[test]
    fn the_first_write_is_judged_by_the_record() {
        let (prober, release, thread) = stalling(0);
        let age = DiskProbe::new(RECORD, ns(REPLACE))
            .unwrap()
            .drain_age_ns()
            .unwrap();
        assert_eq!(prober.drain_age().unwrap(), Duration::from_nanos(age));
        wait_for(&prober, Level::Drain, Duration::from_secs(10));
        assert!(!prober.schedulable());
        prober.stop();
        release.send(()).unwrap();
        thread.join().unwrap().unwrap();
    }

    /// AC-T7.10.3, hostile input: do start the probe on a directory that doesn't exist; expect a typed
    /// refusal naming the record, not a panic.
    #[test]
    fn a_missing_state_dir_is_refused_typed() {
        let dir = tempfile::tempdir().unwrap();
        let err = DiskProber::start(
            &dir.path().join("absent"),
            Duration::from_millis(100),
            RECORD,
            REPLACE,
        );
        assert!(matches!(err, Err(ProbeError::Record { .. })));
    }

    /// A real disk answers: do probe a temporary directory judged by this host class's record across
    /// 20 periods; expect it never drained, the node schedulable, and the record one byte. A host class
    /// with no record is refused, typed, naming it (record its distribution first).
    #[test]
    fn a_disk_that_answers_is_never_drained() {
        let record = match recorded() {
            Ok(record) => record,
            Err(ProbeError::NoRecord(os)) => {
                assert_eq!(os, std::env::consts::OS);
                return;
            }
            Err(other) => panic!("{other}"),
        };
        let dir = tempfile::tempdir().unwrap();
        let p = DiskProber::start(dir.path(), Duration::from_millis(10), record, REPLACE).unwrap();
        for _ in 0..20 {
            assert_ne!(p.level().unwrap(), Level::Drain);
            pause(Duration::from_millis(10));
        }
        assert!(p.schedulable());
        let len = std::fs::metadata(dir.path().join(".hex-probe"))
            .unwrap()
            .len();
        assert_eq!(len, 1);
    }
}
