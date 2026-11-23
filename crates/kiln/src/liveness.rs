//! AD-1 P1.1. All session and database groups on a physical node share this detector.
use anyhow::{Context, Result, ensure};
use hyper_liveness::{Change, Liveness, Settings, Write};
use hyper_timing::Exposure;
use parking_lot::Mutex;
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::sync::{Arc, OnceLock, Weak};
use std::time::Duration;

#[derive(Clone, Copy, Debug)]
pub enum Event {
    Ready,
    Change(Change),
    Timing(u64),
    LocalDisk,
}
type Listener = Arc<dyn Fn(Event) + Send + Sync>;

#[derive(Default)]
struct Elections {
    groups: BTreeMap<u128, Duration>,
    total_ns: u128,
}
impl Elections {
    fn mean(&self) -> Option<Duration> {
        let ns = self.total_ns.checked_div(self.groups.len() as u128)?;
        Some(Duration::from_nanos(u64::try_from(ns).unwrap_or(u64::MAX)))
    }
}
#[derive(Default)]
struct PendingWrites {
    by_group: BTreeMap<u128, u64>,
    by_time: BTreeSet<(u64, u128)>,
}
pub struct Shared {
    generation: std::sync::atomic::AtomicU64,
    revision: std::sync::atomic::AtomicU64,
    #[cfg(test)]
    simulation_ns: std::sync::atomic::AtomicU64,
    local: u64,
    pub(crate) registrar: OnceLock<String>,
    detector: Mutex<Option<Liveness>>,
    listeners: arc_swap::ArcSwap<Vec<(u64, Listener)>>,
    publication: Mutex<()>,
    elections: Mutex<BTreeMap<u64, Elections>>,
    next: std::sync::atomic::AtomicU64,
    pending: Mutex<PendingWrites>,
    wake: Mutex<Option<Weak<crate::rt::Notify>>>,
}

pub struct Subscription {
    shared: Weak<Shared>,
    key: u64,
}
impl Drop for Subscription {
    fn drop(&mut self) {
        if let Some(s) = self.shared.upgrade() {
            s.listeners.rcu(|listeners| {
                Arc::new(listeners.iter().filter(|(key, _)| *key != self.key).cloned().collect())
            });
        }
    }
}

impl Shared {
    pub fn get(local: u64) -> Arc<Self> {
        static NODES: OnceLock<Mutex<BTreeMap<u64, Weak<Shared>>>> = OnceLock::new();
        let mut nodes = NODES.get_or_init(Default::default).lock();
        if let Some(s) = nodes.get(&local).and_then(Weak::upgrade) {
            return s;
        }
        nodes.retain(|_, s| s.strong_count() != 0);
        let shared = Self::isolated(local);
        nodes.insert(local, Arc::downgrade(&shared));
        shared
    }
    pub fn isolated(local: u64) -> Arc<Self> {
        Arc::new(Self {
            generation: Default::default(),
            revision: Default::default(),
            #[cfg(test)]
            simulation_ns: Default::default(),
            local,
            registrar: OnceLock::new(),
            detector: Mutex::new(None),
            listeners: Default::default(),
            publication: Default::default(),
            elections: Default::default(),
            next: Default::default(),
            pending: Default::default(),
            wake: Default::default(),
        })
    }
    #[cfg(test)]
    pub fn set_test_clock(&self, now:u64) { self.simulation_ns.store(now, std::sync::atomic::Ordering::Relaxed); }
    pub fn generation(&self)->u64 { self.generation.load(std::sync::atomic::Ordering::Acquire) }
    pub fn now_ns(&self) -> u64 {
        #[cfg(test)]
        {
            let simulated = self.simulation_ns.load(std::sync::atomic::Ordering::Relaxed);
            if simulated != 0 { return simulated; }
        }
        hyper_rt::machine::clock::shard_clock_ns()
    }
    pub fn with_generation<R>(&self, generation:u64, work:impl FnOnce(&mut Liveness)->R)->Option<R> {
        let mut live = self.detector.lock();
        if self.generation() != generation { return None; }
        live.as_mut().map(work)
    }
    pub fn with<R>(&self, work: impl FnOnce(&mut Liveness) -> R) -> Option<R> {
        self.detector.lock().as_mut().map(work)
    }
    pub fn subscribe(self: &Arc<Self>, f: Listener) -> Subscription {
        let key = self.next.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        self.listeners.rcu(|listeners| {
            let mut updated = Vec::with_capacity(listeners.len() + 1);
            updated.extend(listeners.iter().cloned());
            updated.push((key, Arc::clone(&f)));
            Arc::new(updated)
        });
        Subscription {
            shared: Arc::downgrade(self),
            key,
        }
    }
    pub fn notify(&self, event: Event) {
        let listeners = self.listeners.load();
        for (_, listener) in listeners.iter() {
            listener(event);
        }
    }
    pub(super) fn publish(&self, generation: u64, event: Event) {
        let _one = self.publication.lock();
        if self.with_generation(generation, |_| ()).is_some() { self.notify(event); }
    }
    fn retire(&self, generation: u64) {
        let _one = self.publication.lock();
        {
            let mut detector = self.detector.lock();
            if self.generation() != generation || detector.take().is_none() {return;}
        }
        self.wake.lock().take();
        self.notify(Event::Ready);
    }
    pub fn changed(&self) {
        self.revision.fetch_add(1, std::sync::atomic::Ordering::Release);
        if let Some(w) = self.wake.lock().as_ref().and_then(Weak::upgrade) {
            w.notify();
        }
    }
    pub(super) fn revision(&self) -> u64 { self.revision.load(std::sync::atomic::Ordering::Acquire) }
    pub fn report_io(&self, group: u128, flushed: Option<(u64, u64)>, pending: Option<u64>) {
        let mut io = self.pending.lock();
        let previous = io.by_group.get(&group).copied();
        if previous == pending && flushed.is_none() {
            return;
        }
        if previous != pending {
            if let Some(since) = previous { io.by_time.remove(&(since, group)); }
            if let Some(since) = pending {
                io.by_time.insert((since, group));
                io.by_group.insert(group, since);
            } else {
                io.by_group.remove(&group);
            }
        }
        self.with(|live| {
            if let Some((start, end)) = flushed {
                live.on_durable(Write::Log, start, end);
            }
            if pending != previous {
                live.on_pending(io.by_time.first().map(|(since, _)| *since));
            }
        });
        drop(io);
        self.changed();
    }
    pub fn set_group_election(&self, group: u128, peer: u64, span: Option<Duration>) {
        let mut peers = self.elections.lock();
        if peers.get(&peer).and_then(|s| s.groups.get(&group)).copied() == span { return; }
        let spans = peers.entry(peer).or_default();
        let before = spans.mean();
        if let Some(old) = spans.groups.remove(&group) {
            spans.total_ns -= old.as_nanos();
        }
        if let Some(span) = span {
            spans.groups.insert(group, span);
            spans.total_ns = spans.total_ns.saturating_add(span.as_nanos());
        }
        let Some(mean) = spans.mean() else {
            peers.remove(&peer);
            return;
        };
        if before == Some(mean) { return; }
        let _ = self.with(|l| l.set_election(peer, mean));
        drop(peers);
        self.changed();
    }
    pub fn forget_group_io(&self, group: u128) {
        self.report_io(group, None, None);
    }
    fn initialize(&self, settings: Settings) -> Result<u64> {
        ensure!(settings.local == self.local, "liveness identity mismatch");
        let mut l = Liveness::new(settings)?;
        l.set_disk_apart(true);
        let _one = self.publication.lock();
        let generation = {
            let mut elections = self.elections.lock();
            let writes = self.pending.lock();
            let mut detector = self.detector.lock();
            let generation = self.generation().checked_add(1).context("detector generation exhausted")?;
            elections.clear();
            l.on_pending(writes.by_time.first().map(|(at,_)|*at));
            *detector = Some(l);
            self.generation.store(generation,std::sync::atomic::Ordering::Release);
            generation
        };
        self.notify(Event::Ready);
        Ok(generation)
    }
    #[cfg(test)]
    pub fn initialize_for_test(&self, settings: Settings) -> Result<()> {
        self.initialize(settings).map(|_|())
    }
}

fn run_lock(dir: &Path) -> Result<std::fs::File> {
    lock_file(&dir.join("run.lock"))
}

fn lock_file(path: &Path) -> Result<std::fs::File> {
    let lock = std::fs::OpenOptions::new().create(true).truncate(false).read(true).write(true).open(path)?;
    lock.lock()?;
    Ok(lock)
}

#[cfg(test)]
fn advance_run(dir: &Path) -> Result<u64> {
    let _lock = run_lock(dir)?;
    advance_run_locked(dir)
}
fn current_run(dir: &Path) -> Result<u64> {
    Ok(hyper_block::record::read(&dir.join("run"), size_of::<u64>())?
        .map(|b| -> Result<u64> {
            Ok(u64::from_le_bytes(
                b.as_slice()
                    .try_into()
                    .context("invalid liveness run counter")?,
            ))
        })
        .transpose()?
        .unwrap_or_default())
}

fn advance_run_locked(dir: &Path) -> Result<u64> {
    let path = dir.join("run");
    publish_next_run(current_run(dir)?, |run| {
        hyper_block::record::write(&path, &run.to_le_bytes())?;
        Ok(())
    })
}

fn publish_next_run(previous: u64, save: impl FnOnce(u64) -> Result<()>) -> Result<u64> {
    let run = previous
        .checked_add(1)
        .context("liveness run counter exhausted")?;
    save(run)?;
    Ok(run)
}

fn encode_history(history: Exposure) -> [u8; 20] {
    let mut bytes = [0; 20];
    let (sec, tail) = bytes.split_at_mut(8);
    let (ns, failures) = tail.split_at_mut(4);
    sec.copy_from_slice(&history.node_time().as_secs().to_le_bytes());
    ns.copy_from_slice(&history.node_time().subsec_nanos().to_le_bytes());
    failures.copy_from_slice(&history.failures().to_le_bytes());
    bytes
}
fn read_history(path: &Path) -> Result<Exposure> {
    Ok(history_record(path)?.unwrap_or_else(Exposure::new))
}
fn history_record(path: &Path) -> Result<Option<Exposure>> {
    let Some(data) = hyper_block::record::read(path, 20)? else { return Ok(None) };
    let sec = u64::from_le_bytes(
        data.get(..8)
            .context("missing exposure seconds")?
            .try_into()?,
    );
    let ns = u32::from_le_bytes(
        data.get(8..12)
            .context("missing exposure nanoseconds")?
            .try_into()?,
    );
    let fails = u64::from_le_bytes(
        data.get(12..)
            .context("missing failure count")?
            .try_into()?,
    );
    ensure!(ns < 1_000_000_000, "invalid exposure nanoseconds");
    Ok(Some(Exposure::of(Duration::new(sec, ns), fails)))
}

#[derive(Default)]
struct PendingDisk {
    history: Option<Exposure>,
    flush: bool,
    closed: bool,
    #[cfg(test)]
    before_write: Option<Box<dyn FnOnce() + Send>>,
}

/// These file locks also order a blue/green handoff across processes. The socket releases
/// its guard when dropped. The writer holds its own until every accepted sample is drained.
struct RunFiles {
    socket: PathBuf,
    writer: PathBuf,
    recovery: PathBuf,
}
impl RunFiles {
    fn at(dir: &Path, run: u64) -> Self {
        Self { socket: dir.join(format!("run-{run}.socket")), writer: dir.join(format!("run-{run}.writer")), recovery: dir.join(format!("run-{run}.recovery")) }
    }
    fn existing(path: &Path) -> Result<Option<std::fs::File>> {
        match std::fs::OpenOptions::new().read(true).write(true).open(path) {
            Ok(file) => Ok(Some(file)),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(e.into()),
        }
    }
    fn wait_if_closed(&self, waiting: &impl Fn()) -> Result<()> {
        if let Some(socket) = Self::existing(&self.socket)? {
            match socket.try_lock() {
                // A deliberately overlapping live replacement keeps the existing run-number fence.
                Err(std::fs::TryLockError::WouldBlock) => return Ok(()),
                Err(std::fs::TryLockError::Error(e)) => return Err(e.into()),
                Ok(()) => {}
            }
        }
        if let Some(writer) = Self::existing(&self.writer)? {
            match writer.try_lock() {
                Err(std::fs::TryLockError::WouldBlock) => { waiting(); writer.lock()?; }
                Err(std::fs::TryLockError::Error(e)) => return Err(e.into()),
                Ok(()) => {}
            }
        }
        Ok(())
    }
    fn remove(&self) {
        for path in [&self.socket, &self.writer, &self.recovery] {
            if let Err(error) = std::fs::remove_file(path) && error.kind() != std::io::ErrorKind::NotFound {
                tracing::warn!(?path, %error, "could not remove an older liveness run guard");
            }
        }
    }
}
pub(super) struct Disk {
    shared: Arc<Shared>,
    pub(super) generation: u64,
    pub(super) run: u64,
    _socket: std::fs::File,
    pending: Arc<(Mutex<PendingDisk>, parking_lot::Condvar)>,
}
impl Disk {
    pub(super) fn open(
        dir: &Path,
        shared: &Arc<Shared>,
        wake: &Arc<crate::rt::Notify>,
    ) -> Result<Self> {
        Self::open_wait(dir, shared, wake, || {})
    }
    fn open_wait(dir: &Path, shared: &Arc<Shared>, wake: &Arc<crate::rt::Notify>, waiting: impl Fn()) -> Result<Self> {
        let dir = dir.join("liveness");
        std::fs::create_dir_all(&dir)?;
        crate::blobs::chunks::sync_dir(dir.parent().context("liveness directory lacks parent")?)?;
        let (_publishing, previous) = loop {
            let run = current_run(&dir)?;
            let previous = RunFiles::at(&dir, run);
            previous.wait_if_closed(&waiting)?;
            // The draining writer needs run.lock to finish. Wait outside that lock, then
            // recheck for another opener that may have published while this one was waiting.
            let publishing = run_lock(&dir)?;
            if current_run(&dir)? == run { break (publishing, previous); }
        };
        let history_path = dir.join("history");
        let history = if let Some(final_history) = history_record(&previous.recovery)? {
            // The prior worker preserved its final sample separately after canonical publication
            // failed. Confirm it here before advancing the run or allowing a heartbeat.
            hyper_block::record::write(&history_path, &encode_history(final_history))?;
            final_history
        } else { read_history(&history_path)? };
        let run = advance_run_locked(&dir)?;
        let files = RunFiles::at(&dir, run);
        let socket = lock_file(&files.socket)?;
        let writer = lock_file(&files.writer)?;
        previous.remove();
        let pending = Arc::new((
            Mutex::new(PendingDisk::default()),
            parking_lot::Condvar::new(),
        ));
        let thread_pending = Arc::clone(&pending);
        let s = Arc::clone(shared);
        let (begin, opened) = std::sync::mpsc::sync_channel(1);
        std::thread::Builder::new()
            .name("kiln-liveness-log".into())
            .spawn(move || {
                let _writer = writer;
                if let Ok(generation) = opened.recv() { disk_worker(dir, run, generation, history, s, thread_pending); }
            })?;
        let generation = shared.initialize(Settings {
            local: shared.local,
            run,
            history,
            max_peers: usize::MAX,
            resolution: Duration::from_nanos(hyper_rt::machine::clock::shard_clock_step_ns().max(1)),
        })?;
        *shared.wake.lock() = Some(Arc::downgrade(wake));
        begin.send(generation).context("liveness log worker stopped during startup")?;
        Ok(Self { shared: Arc::clone(shared), generation, run, _socket: socket, pending })
    }
    pub(super) fn submit(&self, history: Option<Exposure>, flush: bool) {
        if history.is_none() && !flush {
            return;
        }
        let (pending, cv) = &*self.pending;
        let mut p = pending.lock();
        if history.is_some() {
            p.history = history;
        }
        p.flush |= flush;
        cv.notify_one();
    }
}
impl Drop for Disk {
    fn drop(&mut self) {
        self.shared.retire(self.generation);
        let (p, cv) = &*self.pending;
        p.lock().closed = true;
        cv.notify_one();
    }
}
fn disk_worker(
    dir: PathBuf,
    run: u64,
    generation: u64,
    mut saved: Exposure,
    shared: Arc<Shared>,
    queue: Arc<(Mutex<PendingDisk>, parking_lot::Condvar)>,
) {
    let (pending, cv) = &*queue;
    let mut proof_pending = false;
    let mut dirty = false;
    loop {
        let (history, flush, closed) = {
            let mut p = pending.lock();
            while p.history.is_none() && !p.flush && !p.closed {
                cv.wait(&mut p);
            }
            (p.history.take(), std::mem::take(&mut p.flush), p.closed)
        };
        if let Some(h) = history {
            saved = h;
            dirty = true;
        }
        proof_pending |= flush;
        if dirty || proof_pending {
            #[cfg(test)]
            if let Some(before) = { let mut p = pending.lock(); p.before_write.take() } { before(); }
            let start = shared.now_ns();
            let write = || -> Result<bool> {
                let _lock = run_lock(&dir)?;
                if hyper_block::record::read(&dir.join("run"), 8)?.as_deref()
                    != Some(run.to_le_bytes().as_slice())
                {
                    return Ok(false);
                }
                hyper_block::record::write(&dir.join("history"), &encode_history(saved))?;
                Ok(true)
            };
            match write() {
                Ok(false) => break,
                Ok(true) => {
                    dirty = false;
                    if proof_pending {
                        proof_pending = false;
                        shared.with_generation(generation, |l| l.on_durable(Write::Liveness, start, shared.now_ns()));
                        shared.changed();
                    }
                }
                Err(error) => {
                    tracing::error!(%error,"liveness history or log flush failed");
                    if closed && dirty && let Err(error) = hyper_block::record::write(&RunFiles::at(&dir, run).recovery, &encode_history(saved)) {
                        tracing::error!(%error, run, "could not preserve the final liveness sample in its recovery record");
                    }
                }
            }
        }
        if closed {
            break;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ad1_power_loss_while_writing_the_run_counter() -> Result<()> {
        use kiln_sim::{
            disk::{Disk as SimDisk, DiskFaults},
            rng::Rng,
        };
        for cut in 0..=4 {
            let disk = SimDisk::new(
                kiln_seams::clock::SimClock::new(),
                Rng::new(cut),
                DiskFaults::none(),
            );
            let save = |n: u64| -> Result<()> {
                disk.write("/run.new", &n.to_le_bytes())?;
                disk.fsync("/run.new")?;
                disk.rename("/run.new", "/run")?;
                disk.fsync_dir("/")?;
                Ok(())
            };
            let previous = publish_next_run(0, save)?;
            disk.freeze_after(cut);
            let seen = publish_next_run(previous, save).unwrap_or(previous);
            disk.power_loss();
            disk.thaw();
            let restored = u64::from_le_bytes(disk.read("/run")?.as_slice().try_into()?);
            anyhow::ensure!(
                publish_next_run(restored, save)? > seen,
                "crash at {cut} repeated a run"
            );
        }
        let dir = tempfile::tempdir()?;
        let seen = advance_run(dir.path())?;
        std::fs::write(dir.path().join("run.new"), b"partial")?;
        anyhow::ensure!(advance_run(dir.path())? > seen);
        std::fs::write(dir.path().join("run"), b"damaged")?;
        anyhow::ensure!(advance_run(dir.path()).is_err());
        Ok(())
    }
    fn pause_accepted_history(disk: &Disk, history: Exposure) -> Result<std::sync::mpsc::SyncSender<()>> {
        let (entered, paused) = std::sync::mpsc::sync_channel(1);
        let (release, resume) = std::sync::mpsc::sync_channel(1);
        disk.pending.0.lock().before_write = Some(Box::new(move || {
            let _ = entered.send(());
            let _ = resume.recv();
        }));
        disk.submit(Some(history), false);
        paused.recv()?;
        Ok(release)
    }

    #[test]
    fn ad1_immediate_reopen_drains_the_closed_sockets_last_accepted_history() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let wake = Arc::new(crate::rt::Notify::new());
        let old = Disk::open(dir.path(), &Shared::isolated(1), &wake)?;
        let old_run = old.run;
        let final_history = Exposure::of(Duration::new(1209, 54), 11);
        let release = pause_accepted_history(&old, final_history)?;
        drop(old); // The socket must finish dropping while its disk is still stalled.
        let fresh = Shared::isolated(1);
        let new = Disk::open_wait(dir.path(), &fresh, &wake, || { let _ = release.try_send(()); })?;
        ensure!(new.run > old_run && current_run(&dir.path().join("liveness"))? == new.run);
        ensure!(fresh.with(|l| l.history()) == Some(final_history));
        ensure!(read_history(&dir.path().join("liveness/history"))? == final_history);
        Ok(())
    }

    #[test]
    fn ad1_failed_final_history_publication_recovers_before_the_next_run_is_announced() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let path = dir.path().join("liveness");
        let wake = Arc::new(crate::rt::Notify::new());
        let old = Disk::open(dir.path(), &Shared::isolated(1), &wake)?;
        let old_run = old.run;
        let final_history = Exposure::of(Duration::from_secs(1209), 3);
        std::fs::create_dir(path.join("history.new"))?; // Ordinary history publishes cannot rename here.
        old.submit(Some(final_history), true);
        drop(old);
        let fresh = Shared::isolated(1);
        ensure!(Disk::open(dir.path(), &fresh, &wake).is_err());
        ensure!(current_run(&path)? == old_run && fresh.with(|l| l.history()).is_none());
        std::fs::remove_dir(path.join("history.new"))?;
        let new = Disk::open(dir.path(), &fresh, &wake)?;
        ensure!(new.run > old_run && fresh.with(|l| l.history()) == Some(final_history));
        Ok(())
    }

    #[test]
    fn ad1_a_live_socket_can_be_replaced_and_its_stale_writer_cannot_regress_history() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let path = dir.path().join("liveness");
        let wake = Arc::new(crate::rt::Notify::new());
        let old = Disk::open(dir.path(), &Shared::isolated(1), &wake)?;
        let release = pause_accepted_history(&old, Exposure::of(Duration::from_secs(9), 1))?;
        // Retain the old guard's file description so we can observe the actual writer exit.
        let prior_writer = std::fs::File::open(RunFiles::at(&path, old.run).writer)?;
        let original = std::cell::RefCell::new(Some(old));
        let new = Disk::open_wait(dir.path(), &Shared::isolated(1), &wake, || {
            // Unstick the writer if a regression waits on a live generation.
            drop(original.borrow_mut().take());
            let _ = release.try_send(());
        })?;
        let old = original.into_inner().context("replacement waited on a live socket")?;
        ensure!(new.run > old.run && current_run(&path)? == new.run);
        drop(old);
        release.send(())?;
        prior_writer.lock()?;
        ensure!(read_history(&path.join("history"))? == Exposure::new());
        Ok(())
    }

    #[test]
    fn ad1_failure_history_restores_the_exact_exposure_and_failure_count() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let path = dir.path().join("history");
        let h = Exposure::of(Duration::new(1_234_567, 891_011), 13);
        hyper_block::record::write(&path, &encode_history(h))?;
        anyhow::ensure!(read_history(&path)? == h);
        Ok(())
    }
}
