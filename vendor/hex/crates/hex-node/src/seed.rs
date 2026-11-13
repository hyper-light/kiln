//! The node agent's pool of exec seeds (decision 3 of docs/design/runtime-architecture.md, goal 3
//! plan step 2), for the execs its own threads can't start: a rootless node's, whose threads may not
//! choose a container's pid namespace (a node that runs as root starts them straight from the calling
//! thread, `hex_runtime::linux::oci::seed::exec_direct`). A seed is a prepared `hex __oci-seed` process
//! the agent keeps and reuses, so such an exec spawns no process of the agent's and re-execs no `hex`
//! binary per call: the agent hands the exec to an idle seed over a `SOCK_SEQPACKET` socket (the
//! request's bytes with the program's stdio and the init's pidfd as `SCM_RIGHTS` descriptors) and gets
//! back the program's pidfd, which the supervisor watches as it watches an init's (`seed.rs` in
//! `hex-runtime` is the seed's own side).
//!
//! Bounds (CLAUDE.md §2), each refused past by a typed [`Refusal`]:
//! - **seeds**, [`SeedPool::budget`]: the most seed processes one node keeps. Seeds start on demand up
//!   to it; past it an exec waits for a free one.
//! - **waiters**, [`WAITERS`]: the most execs waiting for a free seed at once. Past it an exec is
//!   refused, rather than let the wait grow without bound.
//!
//! A seed that dies, or a round trip that breaks, refuses its exec and is replaced; the agent reaps
//! every seed it started (by pidfd), so none is leaked.

use std::os::fd::{AsFd, BorrowedFd, OwnedFd};
use std::path::PathBuf;
use std::process::Command;
use std::sync::{Condvar, Mutex, MutexGuard};
use std::time::Instant;

use hex_api::Refusal;
use hex_core::patience::Patience;
use hex_core::seed::{Answer, Failed, Request};
use hex_runtime::linux::oci::OciError;
use hex_runtime::linux::oci::seed as rt_seed;
use rustix::process::{Pid, PidfdFlags, WaitId, WaitIdOptions};

/// The most execs that wait for a free seed at once; past it an exec is refused with
/// [`Refusal::Exhausted`]. Large, since a waiting exec holds only a little memory and the usual cause
/// of a full pool is a burst that drains in milliseconds, not a wedge.
pub const WAITERS: usize = 4_096;

/// The exec seeds' wait's name, as the waits census records it.
const ANSWERS: &str = "exec_seeds.answers";

/// A pool of reusable exec seeds, grown on demand to [`budget`](Self::budget).
pub struct SeedPool {
    /// The `hex` binary re-exec'd as `hex __oci-seed`.
    exe: PathBuf,
    budget: usize,
    inner: Mutex<Inner>,
    free: Condvar,
    /// How long an exec waits for a busy seed to free, W*: the slowest an exec has held a seed on this node, a bound
    /// only while seeds free within it (decision 9). Past it the wait follows the busy seeds by their life alone;
    /// before the first hold, and from a run-out to the next hold, from its start ([`SeedPool::acquire`]).
    answers: Patience,
}

#[derive(Default)]
struct Inner {
    idle: Vec<Seed>,
    /// Seeds that exist (idle plus in use plus reserved while one is starting).
    total: usize,
    waiting: usize,
}

/// One seed process: the agent's end of its socket, and the pidfd by which the agent ends and reaps
/// it.
struct Seed {
    socket: OwnedFd,
    pidfd: OwnedFd,
    /// When its current exec took it: its hold is a sample of the pool's wait.
    since: Instant,
}

/// The most exec seeds a node keeps on its host (`sizes`; Ada's rule, 2026-11-06): as many as fit in the
/// node's share for seeds, which are the same `hex __oci-seed` process a create seed is
/// ([`crate::sizes::Sizes::seeds`], at [`crate::sizes::SEED_BYTES`] each), and no more than the calls its
/// socket runs at once ([`crate::sizes::Sizes::workers`]), since an exec holds its seed only for its call.
/// The pool starts none and starts one as an exec finds none idle, so it grows to the execs it has seen at
/// once from the structural minimum, none, and this is only its ceiling.
pub(crate) fn most(sizes: &crate::sizes::Sizes) -> usize {
    sizes.seeds.get().min(sizes.workers)
}

impl SeedPool {
    /// A pool that re-execs `exe` as `hex __oci-seed`, up to [`most`] seeds on the node's host.
    pub fn new(exe: PathBuf, sizes: &crate::sizes::Sizes) -> Self {
        Self::with_budget(exe, most(sizes))
    }

    /// A pool bounded to `budget` seeds.
    pub fn with_budget(exe: PathBuf, budget: usize) -> Self {
        Self {
            exe,
            budget: budget.max(1),
            answers: Patience::measuring(ANSWERS),
            inner: Mutex::new(Inner::default()),
            free: Condvar::new(),
        }
    }

    fn lock(&self) -> Result<MutexGuard<'_, Inner>, Refusal> {
        self.inner
            .lock()
            .map_err(|_| Refusal::Broken("the seed pool's lock was poisoned".to_owned()))
    }

    /// Runs `req` with `stdio` in an idle seed (starting one, or waiting for one, within the bounds),
    /// the program joining the namespaces of the container init `init` (its pidfd), and returns the
    /// started program's pid and pidfd for the supervisor to own. The caller holds the supervisor's
    /// spawning lock across this and the own, so the program — the agent's child — isn't reaped by a
    /// sweep before it is owned, and closes its own `stdio` after, so the program's streams end when it
    /// does.
    pub fn exec(
        &self,
        req: &Request,
        stdio: [BorrowedFd<'_>; 3],
        init: OwnedFd,
    ) -> Result<(u32, OwnedFd), Refusal> {
        let mut seed = self.acquire()?;
        match round_trip(&mut seed, req, stdio, init) {
            Ok(started) => {
                self.release(seed);
                Ok(started)
            }
            // The seed is still good (it refused a bad request or a start it couldn't do): back to the
            // pool, the refusal to the caller.
            Err(Trip::Refused(r)) => {
                self.release(seed);
                Err(r)
            }
            // The seed is unusable: end it, make room for a replacement, refuse this exec.
            Err(Trip::Broken(r)) => {
                self.discard(seed);
                Err(r)
            }
        }
    }

    /// An idle seed, a freshly started one if the pool may grow, or a wait for one freed; refused if too many already
    /// wait. The wait, as decision 9 waits (the person's ruling of 2026-11-13):
    /// - **within W***, the slowest hold seen, a seed freed ends it;
    /// - **past W*** its run-out is told to the census, once and with no sample, and it follows the busy seeds by their
    ///   life alone, as at every local site (docs/research/runtime/learned-waits.md): a seed freed, or a slot let go by
    ///   a seed's end ([`SeedPool::discard`]), ends it, however long that takes;
    /// - **with no W*** (before the first hold, and from a run-out to the next hold) it follows them from its start.
    fn acquire(&self) -> Result<Seed, Refusal> {
        let mut inner = self.lock()?;
        let began = Instant::now();
        let mut bound = self.answers.deadline();
        loop {
            if let Some(mut seed) = inner.idle.pop() {
                seed.since = Instant::now();
                return Ok(seed);
            }
            if inner.total < self.budget {
                // Reserve the slot, start the seed without the lock (a fork and exec), then take the
                // slot back on failure so the count stays true.
                inner.total = inner.total.saturating_add(1);
                drop(inner);
                let started = self.start();
                if started.is_err() {
                    let mut inner = self.lock()?;
                    inner.total = inner.total.saturating_sub(1);
                    self.free.notify_one();
                }
                return started;
            }
            if inner.waiting >= WAITERS {
                return Err(Refusal::Exhausted {
                    what: "execs waiting for a seed",
                    bound: WAITERS,
                });
            }
            let now = began.elapsed();
            if let Some(within) = bound
                && now >= within
            {
                // Past W*: no sample (Karn's point), but the census is told, once, and the exec waits on.
                self.answers.ran_out(within);
                bound = None;
            }
            fn poisoned<T>(_: T) -> Refusal {
                Refusal::Broken("the seed pool's lock was poisoned".to_owned())
            }
            inner.waiting = inner.waiting.saturating_add(1);
            inner = match bound {
                Some(within) => {
                    self.free
                        .wait_timeout(inner, within.saturating_sub(now))
                        .map_err(poisoned)?
                        .0
                }
                None => self.free.wait(inner).map_err(poisoned)?,
            };
            inner.waiting = inner.waiting.saturating_sub(1);
        }
    }

    /// Returns a healthy seed to the pool and wakes one waiter.
    fn release(&self, seed: Seed) {
        self.answers.answered(seed.since.elapsed());
        if let Ok(mut inner) = self.lock() {
            inner.idle.push(seed);
            self.free.notify_one();
        }
    }

    /// Ends and reaps an unusable seed, frees its slot, and wakes a waiter (which may start a fresh one).
    fn discard(&self, seed: Seed) {
        end_seed(&seed);
        if let Ok(mut inner) = self.lock() {
            inner.total = inner.total.saturating_sub(1);
            self.free.notify_one();
        }
    }

    /// Starts a seed: a `hex __oci-seed` child of the agent, holding one end of a fresh socket; the
    /// agent keeps the other and the child's pidfd.
    fn start(&self) -> Result<Seed, Refusal> {
        let (ours, theirs) = rt_seed::socketpair()?;
        // The seed's end rides in as its stdin, which the spawn duplicates into the child alone: it stays
        // close-on-exec here, so no process another thread of the agent starts meanwhile inherits it, as
        // one would in the window a cleared flag leaves (CVE-2024-21626's class). The seed keeps only its
        // socket; every other inherited descriptor closes on its re-exec, as the init's do.
        let mut cmd = Command::new(&self.exe);
        cmd.arg("__oci-seed")
            .stdin(std::process::Stdio::from(theirs))
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null());
        let child = cmd
            .spawn()
            .map_err(|e| Refusal::os("starting an exec seed", e))?;
        let pid = i32::try_from(child.id())
            .map_err(|_| Refusal::os("starting an exec seed", "a pid out of range"))?;
        let raw = Pid::from_raw(pid)
            .ok_or_else(|| Refusal::os("starting an exec seed", "the seed's pid isn't a pid"))?;
        let pidfd = rustix::process::pidfd_open(raw, PidfdFlags::empty())
            .map_err(|e| Refusal::os("opening a seed's pidfd", e))?;
        Ok(Seed {
            since: Instant::now(),
            socket: ours,
            pidfd,
        })
    }
}

impl Drop for SeedPool {
    fn drop(&mut self) {
        if let Ok(mut inner) = self.inner.lock() {
            for seed in inner.idle.drain(..) {
                end_seed(&seed);
            }
        }
    }
}

/// Ends a seed (KILL by its pidfd) and reaps it, so no seed the agent started is left running or a
/// zombie. A seed that already exited (its socket closed) takes a harmless failed signal.
fn end_seed(seed: &Seed) {
    let _ = rustix::process::pidfd_send_signal(seed.pidfd.as_fd(), rustix::process::Signal::KILL);
    let _ = rustix::process::waitid(WaitId::PidFd(seed.pidfd.as_fd()), WaitIdOptions::EXITED);
}

/// Why a round trip with a seed didn't start the program.
enum Trip {
    /// The seed refused by type and is still usable.
    Refused(Refusal),
    /// The seed is unusable (its socket broke, or it says its own machinery failed): replace it.
    Broken(Refusal),
}

/// Sends the request, its stdio and the init's pidfd to the seed and reads the answer: the program's pid
/// and pidfd, or a refusal.
fn round_trip(
    seed: &mut Seed,
    req: &Request,
    stdio: [BorrowedFd<'_>; 3],
    init: OwnedFd,
) -> Result<(u32, OwnedFd), Trip> {
    let [stdin, stdout, stderr] = stdio;
    let fds = [stdin, stdout, stderr, init.as_fd()];
    // The agent↔seed wire lives once in `hex-runtime`'s seed module, used by the seed, its
    // intermediate and here (CLAUDE.md §13). A broken socket is the seed's death: take it for broken. A
    // program too big for the wire was never sent, and the seed is whole.
    rt_seed::send_request(seed.socket.as_fd(), req, fds).map_err(|e| match e {
        e @ OciError::Program(_) => Trip::Refused(Refusal::from(e)),
        e => Trip::Broken(Refusal::from(e)),
    })?;
    // The seed holds its own copy of the init's pidfd now.
    drop(init);
    let (answer, pidfd) =
        rt_seed::recv_answer(seed.socket.as_fd()).map_err(|e| Trip::Broken(Refusal::from(e)))?;
    match answer {
        Answer::Started { pid } => match pidfd {
            Some(pidfd) => Ok((pid, pidfd)),
            None => Err(Trip::Broken(Refusal::Broken(
                "a seed said a program started but sent no pidfd".to_owned(),
            ))),
        },
        // Only a pruned create's answer names a mount namespace; an exec seed sending one is broken.
        Answer::StartedIn { .. } => Err(Trip::Broken(Refusal::Broken(
            "an exec seed answered as a create seed".to_owned(),
        ))),
        // A seed started under a lower stack limit than the node's reading refuses the program by its own
        // (HEX-RT-14): the node reads its limit again, once, on this refusal alone, and holds the next exec
        // to it before any seed is asked.
        Answer::Refused {
            failed: Failed::Program,
            why,
        } => {
            hex_runtime::linux::process::relearn_stack();
            Err(Trip::Refused(Refusal::Invalid {
                what: "program",
                why,
            }))
        }
        Answer::Refused { failed, why } => {
            let refusal = Refusal::Os {
                op: "exec in a container",
                why,
            };
            match failed {
                Failed::Seed => Err(Trip::Broken(refusal)),
                // `Unable` is a create's outcome (no clone3 into a cgroup), which an exec never meets;
                // were it to, the seed is whole, so the exec is refused by type, not the seed replaced.
                Failed::Request | Failed::Start | Failed::Unable | Failed::Program => {
                    Err(Trip::Refused(refusal))
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::sync::Arc;
    use std::time::Duration;

    /// Longest a test waits for a fact it needs: a bound, so a pool that never makes it true fails the test instead
    /// of hanging it.
    const PATIENCE: Duration = Duration::from_secs(30);

    /// A seed for the tests: descriptors that are no seed's, which the pool only holds, hands out and ends.
    fn stand_in() -> Seed {
        let null = || OwnedFd::from(std::fs::File::open("/dev/null").unwrap());
        Seed {
            socket: null(),
            pidfd: null(),
            since: Instant::now(),
        }
    }

    /// A pool of one seed, out with an exec, whose wait has learned a W* of `learned`: the next exec waits for a seed
    /// freed.
    fn busy_pool(learned: Duration) -> Arc<SeedPool> {
        let pool = SeedPool::with_budget(PathBuf::from("/nonexistent/hex-for-the-test"), 1);
        pool.answers.answered(learned);
        pool.inner.lock().unwrap().total = 1;
        Arc::new(pool)
    }

    /// Decision 9 (original; the person's ruling of 2026-11-13: W* is a bound only while answers come within it): an
    /// exec waiting for a busy seed was refused at W*, the slowest hold seen, though its seed was still serving. Do: a
    /// pool of one seed out with an exec, W* 1 s; an exec waits, and once its wait has run out (told) or it was
    /// refused, the seed is freed. Expect: the exec handed the seed.
    #[test]
    fn original_an_exec_waiting_past_the_exec_seeds_w_star_waits_on_for_a_seed_freed() {
        let pool = busy_pool(Duration::from_secs(1));
        let waiter = {
            let pool = Arc::clone(&pool);
            std::thread::spawn(move || pool.acquire().map(drop))
        };
        let asked = Instant::now();
        while pool.answers.deadline().is_some() && !waiter.is_finished() {
            assert!(
                asked.elapsed() < PATIENCE,
                "the exec's wait neither ran out nor ended"
            );
            std::thread::park_timeout(Duration::from_millis(1));
        }
        pool.release(stand_in());
        let got = waiter.join().unwrap();
        assert!(got.is_ok(), "the exec was refused at W*: {got:?}");
    }

    /// Decision 9 (the person's ruling of 2026-11-13: W* is a bound only while answers come within it; past it the wait
    /// follows its party, the busy seeds, by their life alone, as at every local site): an exec past W* was refused
    /// once a whole window passed with no seed freed, though its seed still served. Do: a pool of one seed out with an
    /// exec, W* 1 ms; an exec waits; once its run-out is told, the seed stays out for a hundred windows of W*, or until
    /// the exec is answered; then it is freed. Expect: the exec not answered while the seed is out, then handed the
    /// seed freed.
    #[test]
    fn an_exec_past_w_star_waits_on_while_its_seed_serves_however_long_none_frees() {
        let learned = Duration::from_millis(1);
        let pool = busy_pool(learned);
        let waiter = {
            let pool = Arc::clone(&pool);
            std::thread::spawn(move || pool.acquire().map(drop))
        };
        let asked = Instant::now();
        while pool.answers.deadline().is_some() && !waiter.is_finished() {
            assert!(
                asked.elapsed() < PATIENCE,
                "the exec's wait neither ran out nor ended"
            );
            std::thread::park_timeout(learned);
        }
        let told = Instant::now();
        while !waiter.is_finished() && told.elapsed() < learned.saturating_mul(100) {
            std::thread::park_timeout(learned);
        }
        if waiter.is_finished() {
            panic!(
                "the exec was answered with its seed still out: {:?}",
                waiter.join().unwrap()
            );
        }
        pool.release(stand_in());
        let got = waiter.join().unwrap();
        assert!(
            got.is_ok(),
            "the exec was refused with a seed freed: {got:?}"
        );
    }

    /// Decision 9 (no record is read; AC-T7.1.3): an exec's wait for a busy seed to free follows the busy seeds until
    /// an exec has held one. Do: open an exec seed pool and ask its wait before any exec. Expect: no bound, no picked
    /// 30 s and no record.
    #[test]
    fn an_execs_wait_for_a_seed_follows_the_seeds_until_an_exec_has_held_one() {
        let pool = SeedPool::with_budget(PathBuf::from("/nonexistent/hex-for-the-test"), 1);
        assert_eq!(pool.answers.deadline(), None);
    }
}
