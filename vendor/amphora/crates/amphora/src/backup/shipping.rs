//! The thread that backs a [`crate::database::Database`]'s local writer up (CP-T1.12;
//! docs/design/backup.md, "The database's shipping thread").
//!
//! It wakes on each commit the store makes, never on a timer: a round ships what became durable
//! while the last ran, then copies one chunk of a base while one is due, so its cadence follows
//! the log's rate and the store's put time. It ships through a reader on the writer's engine, so
//! the writer never waits on the store. When the database ends it ships once more and ends.
//!
//! Given a scratch path ([`Archive::check`]) it restores and checks its own archive on the
//! archive's cadence ([`super::check_due`]): against the source's rows at a second, digested from a
//! reader whose snapshot lines up with that second's last commit ([`super::Marks::quiet_second`]).

use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Receiver, TryRecvError, TrySendError};
use std::sync::{Arc, Condvar, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use amphora_api::blobs::Blobs;

use super::{
    BaseCopy, Check, CheckCounters, Clock, Hooks, RowDigest, Shipped, Shipper, check_due, digest,
    newest_second, plan, restore_and_check_at,
};
use crate::db::Db;
use crate::store::Watcher;

/// Where and how a database is backed up ([`crate::database::Database::open_backed_up`]).
#[derive(Clone)]
pub struct Archive {
    /// The object store: Mantle, through kiln's adapter, in production.
    pub blobs: Arc<dyn Blobs>,
    /// The archive's prefix in the store: one database's.
    pub prefix: String,
    /// When each commit was made ([`system_clock`] in production).
    pub clock: Clock,
    /// The memory a base's chunk may take ([`Shipper::begin_base`]).
    pub memory: usize,
    /// How long one request to the store may take: past it the round is given up and tried again
    /// at the next commit. The caller states it, as the store's own answer time is the caller's
    /// to know (Needs Ada's word: no measured default yet).
    pub patience: Duration,
    /// Where the thread restores its archive to check it (T-T1.12.C3): a new file there each
    /// check, removed once it passes and kept on a failure to look into. None: no checks. The
    /// caller's, as the restore takes as much disk as the database.
    pub check: Option<PathBuf>,
}

/// Nanoseconds since the Unix epoch by the system's clock: 0 before the epoch, the most a `u64`
/// holds past its range (both held back by [`super::Marks`], which never stamps backwards).
#[must_use]
pub fn system_clock() -> Clock {
    Arc::new(|| {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| u64::try_from(d.as_nanos()).unwrap_or(u64::MAX))
    })
}

/// What the shipping thread has done.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Status {
    /// Rounds that ended without failing.
    pub rounds: u64,
    /// All it shipped of the log.
    pub shipped: Shipped,
    /// Bases it ended.
    pub bases: u64,
    /// Rounds that failed, and the latest failure.
    pub failures: u64,
    pub last_failure: Option<String>,
    /// Whether the thread ended (its database did).
    pub ended: bool,
    /// The latest restore-and-check's outcome ([`Archive::check`]).
    pub last_check: Option<Check>,
}

/// A handle on a database's shipping thread: what it has done, and waits on it.
#[derive(Clone)]
pub struct Shipping {
    status: Arc<(Mutex<Status>, Condvar)>,
    counters: Arc<CheckCounters>,
}

impl Shipping {
    fn locked(&self) -> anyhow::Result<MutexGuard<'_, Status>> {
        self.status
            .0
            .lock()
            .map_err(|_| anyhow::anyhow!("the backup's status lock was poisoned"))
    }

    /// The checks the thread ran, by result: `amphora_backup_checks_total` ([`super::CHECK_METRIC`]).
    #[must_use]
    pub fn checks(&self) -> &CheckCounters {
        &self.counters
    }

    /// What the thread has done so far.
    pub fn status(&self) -> anyhow::Result<Status> {
        Ok(self.locked()?.clone())
    }

    /// Waits until `done` holds of the thread's status, until `until` at most (refused then).
    pub fn wait(&self, until: Instant, done: impl Fn(&Status) -> bool) -> anyhow::Result<Status> {
        let mut status = self.locked()?;
        loop {
            if done(&status) {
                return Ok(status.clone());
            }
            let left = until.saturating_duration_since(Instant::now());
            if left.is_zero() {
                anyhow::bail!("the backup's status didn't reach what was waited for in time");
            }
            status = self
                .status
                .1
                .wait_timeout(status, left)
                .map_err(|_| anyhow::anyhow!("the backup's status lock was poisoned"))?
                .0;
        }
    }

    fn record(&self, change: impl FnOnce(&mut Status)) {
        if let Ok(mut status) = self.locked() {
            change(&mut status);
        }
        self.status.1.notify_all();
    }
}

/// A backup made ready on the writer as it opens, its thread not started yet.
pub(crate) struct Prepared {
    archive: Archive,
    shipper: Shipper,
    reader: Db,
    hooks: Arc<Hooks>,
    /// The reader a check's snapshot of the source is taken on, apart from the shipping reader.
    checker: Option<Db>,
    counters: Arc<CheckCounters>,
    /// The latest check's base and the log it replayed, the log shipped since
    /// ([`super::check_due`]), and a quiet snapshot's second and rows waiting to be checked.
    last_check: Option<(u64, u64)>,
    log_since_check: u64,
    pending_check: Option<(u64, RowDigest)>,
}

impl Prepared {
    /// The writer's stamps.
    pub(crate) fn hooks(&self) -> Arc<Hooks> {
        Arc::clone(&self.hooks)
    }

    /// Backs `db`, the writer, up to `archive`: its commits stamped from now on, its log started
    /// again only once shipped ([`Db::back_up`]), and a shipper carrying on from what the archive
    /// holds, on a reader of the same engine.
    pub(crate) fn on(db: &mut Db, archive: Archive, until: Instant) -> anyhow::Result<Self> {
        let hooks = Hooks::new(Arc::clone(&archive.clock), db.log_kept_frames());
        db.back_up(Arc::clone(&hooks));
        let reader = db.reader()?;
        // The shipping reader may be the engine's last connection to close: its close must not
        // empty a log the backup hasn't shipped either.
        reader.hold_log();
        let checker = match archive.check {
            Some(_) => {
                let checker = db.reader()?;
                checker.hold_log();
                Some(checker)
            }
            None => None,
        };
        let shipper = Shipper::open(
            &archive.prefix,
            Arc::clone(&hooks),
            archive.blobs.as_ref(),
            &reader,
            until,
        )?;
        Ok(Self {
            archive,
            shipper,
            reader,
            hooks,
            checker,
            counters: Arc::default(),
            last_check: None,
            log_since_check: 0,
            pending_check: None,
        })
    }

    /// Starts the thread, woken by each commit `on_commit` reports. It ends once the store drops
    /// its watchers, as the database ends.
    pub(crate) fn start(self, on_commit: impl FnOnce(Watcher)) -> anyhow::Result<Shipping> {
        // One wake pending at most: a commit while one waits is shipped by the same round.
        let (wake, woken) = mpsc::sync_channel::<()>(1);
        // The first round runs at once: what the database committed as it opened is shipped, and
        // its first base begins, without waiting for a commit.
        wake.try_send(())?;
        on_commit(Box::new(move || {
            !matches!(wake.try_send(()), Err(TrySendError::Disconnected(())))
        }));
        let shipping = Shipping {
            status: Arc::default(),
            counters: Arc::clone(&self.counters),
        };
        let told = shipping.clone();
        std::thread::Builder::new()
            .name("amphora-backup".into())
            .spawn(move || self.run(&woken, &told))?;
        Ok(shipping)
    }

    fn run(mut self, woken: &Receiver<()>, shipping: &Shipping) {
        let mut copy: Option<BaseCopy> = None;
        loop {
            match self.round(&mut copy) {
                Ok((shipped, base_ended)) => shipping.record(|s| {
                    s.rounds = s.rounds.saturating_add(1);
                    s.shipped.segments = s.shipped.segments.saturating_add(shipped.segments);
                    s.shipped.bytes = s.shipped.bytes.saturating_add(shipped.bytes);
                    if base_ended {
                        s.bases = s.bases.saturating_add(1);
                    }
                }),
                Err(e) => {
                    let why = format!("{e:#}");
                    tracing::warn!(target: "amphora.backup", event = "backup.round_failed", prefix = %self.archive.prefix, error = %why, "a backup round failed: tried again at the next commit");
                    // A base under way starts over: its chunks may straddle the failure.
                    copy = None;
                    shipping.record(|s| {
                        s.failures = s.failures.saturating_add(1);
                        s.last_failure = Some(why);
                    });
                }
            }
            // A check's failure to run is the round's, but leaves a base under way as it was.
            match self.checks() {
                Ok(Some(check)) => shipping.record(|s| s.last_check = Some(check)),
                Ok(None) => {}
                Err(e) => {
                    let why = format!("{e:#}");
                    tracing::warn!(target: "amphora.backup", event = "backup.check_failed_to_run", prefix = %self.archive.prefix, error = %why, "a backup check couldn't run: tried again at the next commit");
                    shipping.record(|s| {
                        s.failures = s.failures.saturating_add(1);
                        s.last_failure = Some(why);
                    });
                }
            }
            // A base under way copies its next chunk at once; otherwise the next commit wakes it.
            let next = if copy.is_some() {
                match woken.try_recv() {
                    Ok(()) | Err(TryRecvError::Empty) => Ok(()),
                    Err(TryRecvError::Disconnected) => Err(()),
                }
            } else {
                woken.recv().map_err(|_| ())
            };
            if next.is_err() {
                break;
            }
        }
        // The database ended: what it committed last is shipped before the thread lets go of it.
        let last = self.ship();
        shipping.record(|s| {
            match last {
                Ok(shipped) => {
                    s.rounds = s.rounds.saturating_add(1);
                    s.shipped.segments = s.shipped.segments.saturating_add(shipped.segments);
                    s.shipped.bytes = s.shipped.bytes.saturating_add(shipped.bytes);
                }
                Err(e) => {
                    s.failures = s.failures.saturating_add(1);
                    s.last_failure = Some(format!("{e:#}"));
                }
            }
            s.ended = true;
        });
    }

    fn until(&self) -> Instant {
        Instant::now()
            .checked_add(self.archive.patience)
            .unwrap_or_else(Instant::now)
    }

    fn ship(&mut self) -> anyhow::Result<Shipped> {
        let until = self.until();
        self.shipper
            .ship(&self.reader, self.archive.blobs.as_ref(), until)
    }

    /// Ships, then moves a base on by one chunk while one is due: what it shipped, and whether a
    /// base ended.
    fn round(&mut self, copy: &mut Option<BaseCopy>) -> anyhow::Result<(Shipped, bool)> {
        let generation = self.shipper.generation();
        let shipped = self.ship()?;
        self.log_since_check = self.log_since_check.saturating_add(shipped.bytes);
        let blobs = self.archive.blobs.as_ref();
        let until = self.until();
        match copy.as_mut() {
            None => {
                // A base begins in a generation the shipper has shipped: after a round that
                // shipped, or once there's nothing left to (`begin_base` refuses otherwise); and
                // just after the log starts again, so the next base copies least (T-T1.12.I4).
                if self.shipper.base_due(None)
                    && self.shipper.ships_current(&self.reader)
                    && plan::base_begins_now(
                        self.shipper.base_needed(),
                        self.shipper.generation() != generation,
                        self.shipper.frames_in_generation(),
                        self.reader.log_kept_frames(),
                    )
                {
                    *copy = Some(self.shipper.begin_base(
                        &self.reader,
                        blobs,
                        self.archive.memory,
                    )?);
                }
                Ok((shipped, false))
            }
            Some(c) => {
                if self.shipper.copy_base(&self.reader, blobs, c, until)? {
                    return Ok((shipped, false));
                }
                let Some(done) = copy.take() else {
                    return Ok((shipped, false));
                };
                self.shipper.end_base(&self.reader, blobs, done, until)?;
                Ok((shipped, true))
            }
        }
    }

    /// Checks a quiet snapshot's second once it settled and the archive holds it whole, then takes
    /// the next snapshot when a check is due (T-T1.12.C3): what a check found, if one ran.
    fn checks(&mut self) -> anyhow::Result<Option<Check>> {
        let check = self.check_pending()?;
        if self.pending_check.is_none()
            && let Some(base) = self.shipper.latest_base()
            && check_due(self.last_check, base, self.log_since_check)
        {
            self.pending_check = self.quiet_digest()?;
        }
        Ok(check)
    }

    /// The source's rows at a second, from a snapshot lined up with that second's last commit
    /// ([`super::Marks::quiet_second`]); none when the log isn't quiet now.
    fn quiet_digest(&mut self) -> anyhow::Result<Option<(u64, RowDigest)>> {
        let Some(checker) = self.checker.as_mut() else {
            return Ok(None);
        };
        let marks = &self.hooks.marks;
        checker.query_transaction(|r| {
            let second = marks.quiet_second(|| {
                // A deferred `BEGIN` takes no snapshot: this first read takes it, under the lock
                // commits are stamped by, and the log's end is read after it.
                r.query("SELECT count(*) FROM sqlite_schema", [], |_| Ok(true))?;
                Ok(r.log_end())
            })?;
            match second {
                Some(second) => Ok(Some((second, digest(r)?))),
                None => Ok(None),
            }
        })
    }

    fn check_pending(&mut self) -> anyhow::Result<Option<Check>> {
        let Some((second, _)) = self.pending_check else {
            return Ok(None);
        };
        match self.hooks.marks.settled(second)? {
            None => return Ok(None),
            Some(false) => {
                self.pending_check = None;
                return Ok(None);
            }
            Some(true) => {}
        }
        let Some(scratch) = self.archive.check.clone() else {
            self.pending_check = None;
            return Ok(None);
        };
        let until = self.until();
        let blobs = self.archive.blobs.as_ref();
        // The archive holds the second whole once a round shipped past its end: until then the
        // snapshot waits for the next round.
        if newest_second(blobs, &self.archive.prefix, until)?.is_none_or(|newest| newest < second) {
            return Ok(None);
        }
        let Some((second, rows)) = self.pending_check.take() else {
            return Ok(None);
        };
        remove_scratch(&scratch)?;
        let source = |s: u64| Ok((s == second).then(|| rows.clone()));
        let check = restore_and_check_at(
            blobs,
            &self.archive.prefix,
            second,
            &scratch,
            &source,
            &self.counters,
            until,
        )?;
        self.last_check = Some(match &check {
            Check::Passed { restored, .. } => {
                remove_scratch(&scratch)?;
                (restored.base, restored.log_bytes)
            }
            // Not passed: checked again at the next base, or once as much log again has shipped.
            _ => (
                self.shipper.latest_base().unwrap_or(0),
                self.log_since_check,
            ),
        });
        self.log_since_check = 0;
        Ok(Some(check))
    }
}

/// Removes a check's scratch database and its log, if there.
fn remove_scratch(scratch: &Path) -> anyhow::Result<()> {
    for suffix in ["", "-wal", "-shm"] {
        let mut path = scratch.as_os_str().to_owned();
        path.push(suffix);
        match std::fs::remove_file(PathBuf::from(path)) {
            Err(e) if e.kind() != std::io::ErrorKind::NotFound => return Err(e.into()),
            _ => {}
        }
    }
    Ok(())
}
