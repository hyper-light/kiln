//! The worker around the bounded pure pools of mounts made ahead: `/dev` tmpfs (goal 5 extended step 3) and
//! proc with its masks and read-only paths, for a pid namespace made ahead (decision 6's update, part 2).
//! Each prepared mount is detached and owned by one create; nothing is cloned or returned after use.
use hex_api::Refusal;
use hex_core::devfs::{Counts, MakeFailure, Pool, Shape};
use hex_runtime::linux::oci::init::{PreparedProc, ProcSources};
use hex_runtime::linux::oci::run::DevMounts;
use hex_runtime::linux::oci::{OciError, devmount::Key, init, procmount};
use std::os::fd::{BorrowedFd, OwnedFd};
use std::sync::{Arc, Condvar, Mutex};
use std::thread::JoinHandle;

pub struct Devfs {
    shared: Arc<Shared>,
    worker: Option<JoinHandle<()>>,
    /// Whether each pool makes ahead, as started: `/dev`'s and the procs'; what a test control restarts from.
    #[cfg(feature = "test-controls")]
    enabled: (bool, bool),
    /// Where each pool's keys and mounts a key start and their bound.
    #[cfg(feature = "test-controls")]
    shape: Shape,
}
struct Shared {
    pool: Mutex<Pool<Key, OwnedFd>>,
    procs: Mutex<Pool<procmount::Key, PreparedProc>>,
    /// What masks bind, opened once for the node's life on a root node (a rootless one can't make the empty
    /// tmpfs): a prepared proc's masks, and every init's masked directories ([`DevMounts::masked_directory`]).
    sources: Option<Arc<ProcSources>>,
    wake: Condvar,
}

/// Makes the proc of a parked init's own pid namespace (decision 8, parked procs) from a root node's mask sources,
/// on the thread that parked it: the tree is the node's alone until a create claims the init.
#[derive(Clone)]
pub struct ProcMaker(Arc<ProcSources>);

impl ProcMaker {
    /// The proc for `pid_namespace` with `key`'s masks and read-only paths on it.
    pub fn make(
        &self,
        pid_namespace: OwnedFd,
        key: &procmount::Key,
    ) -> Result<PreparedProc, OciError> {
        init::prepare_proc_for(pid_namespace, key, &self.0)
    }
}
impl Devfs {
    /// The worker for the `/dev` pool, made ahead where `dev`, and the proc pool where `procs`; a disabled
    /// pool counts every take a disabled miss. A `root` node keeps the mask sources whether or not either pool
    /// makes ahead, since every init's masked directories clone its empty tmpfs.
    /// Each pool's keys and mounts a key start, and are bounded, as `shape` says: from the node's sizes.
    pub fn start(dev: bool, procs: bool, root: bool, shape: Shape) -> Result<Self, Refusal> {
        let sources = if root {
            Some(Arc::new(ProcSources::open().map_err(|e| {
                Refusal::os("opening the prepared procs' mask sources", e)
            })?))
        } else {
            None
        };
        Self::over(sources, dev, procs, shape)
    }

    /// As [`Devfs::start`], with this one's mask sources: a node keeps one empty tmpfs for its life, so every init's
    /// masked directories and every parked proc's clone the same one, whichever pools are on.
    #[cfg(feature = "test-controls")]
    pub fn restart(&self, dev: bool, procs: bool) -> Result<Self, Refusal> {
        Self::over(self.shared.sources.clone(), dev, procs, self.shape)
    }

    fn over(
        sources: Option<Arc<ProcSources>>,
        dev: bool,
        procs: bool,
        shape: Shape,
    ) -> Result<Self, Refusal> {
        let shared = Arc::new(Shared {
            pool: Mutex::new(Pool::new(dev, shape)),
            procs: Mutex::new(Pool::new(procs, shape)),
            sources,
            wake: Condvar::new(),
        });
        let worker = if dev || procs {
            let s = Arc::clone(&shared);
            Some(
                std::thread::Builder::new()
                    .name("hex-devfs".to_owned())
                    .spawn(move || refill(&s))
                    .map_err(|e| Refusal::os("starting the prepared /dev worker", e))?,
            )
        } else {
            None
        };
        Ok(Self {
            shared,
            worker,
            #[cfg(feature = "test-controls")]
            enabled: (dev, procs),
            #[cfg(feature = "test-controls")]
            shape,
        })
    }
    pub fn counts(&self) -> Result<Counts, Refusal> {
        self.shared
            .pool
            .lock()
            .map(|p| p.counts())
            .map_err(|_| Refusal::Broken("the prepared /dev pool lock was poisoned".to_owned()))
    }

    /// What both pools have learned: the most keys either keeps, and the most mounts a key any keeps.
    pub fn learned(&self) -> Result<(usize, usize), Refusal> {
        let poisoned = || Refusal::Broken("a prepared mounts pool's lock was poisoned".to_owned());
        let dev = self.shared.pool.lock().map_err(|_| poisoned())?.learned();
        let procs = self.shared.procs.lock().map_err(|_| poisoned())?.learned();
        Ok((dev.0.max(procs.0), dev.1.max(procs.1)))
    }

    /// Whether the `/dev` pool and the proc pool make ahead, as started, and whether the node is root's.
    #[cfg(feature = "test-controls")]
    pub fn enabled(&self) -> (bool, bool, bool) {
        // A root node's is the one that keeps the mask sources.
        (
            self.enabled.0,
            self.enabled.1,
            self.shared.sources.is_some(),
        )
    }

    /// What makes parked inits' procs, on a root node, which alone has the mask sources.
    pub fn proc_maker(&self) -> Option<ProcMaker> {
        self.shared
            .sources
            .as_ref()
            .map(|s| ProcMaker(Arc::clone(s)))
    }

    /// What the prepared proc pool has done (decision 6's update, part 2).
    pub fn proc_counts(&self) -> Result<Counts, Refusal> {
        self.shared
            .procs
            .lock()
            .map(|p| p.counts())
            .map_err(|_| Refusal::Broken("the prepared proc pool lock was poisoned".to_owned()))
    }
}
impl DevMounts for Devfs {
    fn unpooled(&self, reason: hex_core::devfs::Unpooled) -> Result<(), OciError> {
        self.shared
            .pool
            .lock()
            .map_err(|_| OciError::Broken("the prepared /dev pool lock was poisoned".to_owned()))?
            .unpooled(reason)
            .map_err(|e| OciError::Broken(e.to_string()))
    }

    /// The worker is woken only when the take leaves it work, a key at its low mark (the core's `Pool::take`), not
    /// once a take.
    fn take(&self, key: Key) -> Result<Option<OwnedFd>, OciError> {
        let took = {
            let mut pool = self.shared.pool.lock().map_err(|_| {
                OciError::Broken("the prepared /dev pool lock was poisoned".to_owned())
            })?;
            let before = pool.learned();
            let took = pool
                .take(key)
                .map_err(|e| OciError::Broken(e.to_string()))?;
            if pool.learned() > before {
                crate::sizes::rose();
            }
            took
        };
        drop(took.discarded);
        if took.wake {
            self.shared.wake.notify_one();
        }
        Ok(took.mount)
    }

    fn masked_directory(&self) -> Option<BorrowedFd<'_>> {
        self.shared.sources.as_deref().and_then(ProcSources::empty)
    }

    /// The worker is woken only when the take leaves it work, as a `/dev` take wakes it.
    fn take_proc(&self, key: procmount::Key) -> Result<Option<PreparedProc>, OciError> {
        let took = {
            let mut procs = self.shared.procs.lock().map_err(|_| {
                OciError::Broken("the prepared proc pool lock was poisoned".to_owned())
            })?;
            let before = procs.learned();
            let took = procs
                .take(key)
                .map_err(|e| OciError::Broken(e.to_string()))?;
            if procs.learned() > before {
                crate::sizes::rose();
            }
            took
        };
        drop(took.discarded);
        if took.wake {
            // The worker checks this pool holding `/dev`'s lock, the condition's, so taking that lock before
            // the wake means the wake can't fall between its check and its wait.
            drop(self.shared.pool.lock());
            self.shared.wake.notify_one();
        }
        Ok(took.mount)
    }

    fn unpooled_proc(&self, reason: hex_core::devfs::Unpooled) -> Result<(), OciError> {
        self.shared
            .procs
            .lock()
            .map_err(|_| OciError::Broken("the prepared proc pool lock was poisoned".to_owned()))?
            .unpooled(reason)
            .map_err(|e| OciError::Broken(e.to_string()))
    }
}
impl Drop for Devfs {
    fn drop(&mut self) {
        let held = self.shared.pool.lock().map(|mut p| p.shut_down());
        drop(held);
        let procs = self.shared.procs.lock().map(|mut p| p.shut_down());
        drop(procs);
        self.shared.wake.notify_all();
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}
/// One job at a time, `/dev`'s first, then the procs'. Each pool's lock is held only to take a job and
/// to hand back what it made.
fn refill(shared: &Shared) {
    loop {
        if let Some(job) = match wait_for_work(shared) {
            Work::Dev(job) => Some(job),
            Work::Proc(job) => {
                let made = match &shared.sources {
                    Some(sources) => init::prepare_proc(job.key(), sources),
                    None => Err(OciError::Broken(
                        "a proc job on a node whose proc pool is disabled".to_owned(),
                    )),
                };
                // A failure is an optimization miss, counted, as `/dev`'s: the create makes its own. A
                // kernel that won't hand over a pid namespace with no process in it yet (below Linux 7.1,
                // `goal6-mntns.md` §7) can make none: the pool stops for good rather than fail each take.
                let unsupported = matches!(
                    &made,
                    Err(OciError::Os {
                        op: init::PID_NAMESPACE_AHEAD,
                        ..
                    })
                );
                let Ok(mut procs) = shared.procs.lock() else {
                    return;
                };
                let discarded = procs.complete(
                    job,
                    made.map_err(|_| {
                        if unsupported {
                            MakeFailure::Namespace
                        } else {
                            MakeFailure::Filesystem
                        }
                    }),
                );
                let held = if unsupported {
                    procs.disable()
                } else {
                    Vec::new()
                };
                drop(procs);
                drop((discarded, held));
                None
            }
            Work::Done => return,
        } {
            let k = job.key();
            let made = init::prepare_dev(&k.source, &k.data, k.flags);
            // A preparation failure is an optimization miss: counted, with no retry until a new take.
            // The requesting create always runs ordinary setup when dry, preserving its typed refusal.
            let Ok(mut pool) = shared.pool.lock() else {
                return;
            };
            let discarded = pool.complete(
                job,
                made.map_err(|e| match e {
                    OciError::Os {
                        op: "making a device" | "setting a device's mode",
                        ..
                    } => MakeFailure::Device,
                    OciError::Os {
                        op: "preparing a /dev link",
                        ..
                    } => MakeFailure::Link,
                    _ => MakeFailure::Filesystem,
                }),
            );
            drop(pool);
            drop(discarded);
        }
    }
}

enum Work {
    Dev(hex_core::devfs::Job<Key>),
    Proc(hex_core::devfs::Job<procmount::Key>),
    Done,
}

/// Waits until either pool wants a make, or both are shut, and reserves the job. The wait is on `/dev`'s
/// lock, which every take and shutdown notifies after.
fn wait_for_work(shared: &Shared) -> Work {
    let Ok(mut pool) = shared.pool.lock() else {
        return Work::Done;
    };
    loop {
        if pool.closed() {
            return Work::Done;
        }
        if pool.wants_work()
            && let Some(job) = pool.next_job()
        {
            return Work::Dev(job);
        }
        match shared.procs.lock() {
            Ok(mut procs) => {
                if procs.wants_work()
                    && let Some(job) = procs.next_job()
                {
                    return Work::Proc(job);
                }
            }
            Err(_) => return Work::Done,
        }
        pool = match shared.wake.wait(pool) {
            Ok(p) => p,
            Err(_) => return Work::Done,
        };
    }
}
