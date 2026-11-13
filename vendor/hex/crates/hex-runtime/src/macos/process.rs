//! The `process` class on macOS (docs/design/runtime.md, decision "each OS runs its own boxes
//! natively"): a box is a process group Hex spawns (posix_spawn, a new process group), at background QoS
//! when its declared weight is under the default and unclamped otherwise. macOS has no public cap on a process tree's memory or
//! its process count (docs/research/runtime/native-platforms.md §3), so a watcher thread holds the box's
//! bounds by Hex's own rule ([`hex_core::watch`]): it lists the box's members (`proc_listpids` of its
//! process group), reads each footprint (`proc_pid_rusage`), and ends the box (killpg, then the members'
//! own waiters reap) once it passes a bound. The rule is the pure core; this shell does the listing, the
//! reading, the waiting and the killing, from a timeshare thread on `NOTE_CRITICAL` kqueue
//! timers so a box filling every core can't hold its watcher off (RESULT.md).
//!
//! Each member Hex spawns is also armed with a fatal per-process memory cap at the box's whole bound
//! (`posix_spawnattr_setjetsam_ext`, rootless; `sys`, docs/research/runtime/macos-spawn-jetsam-cap.md):
//! the kernel SIGKILLs it at the bound with no overshoot (native plan step 1's `_ext` backstop). So a box
//! holding at most one armed member is bounded by the kernel, and the watcher reads it only when it
//! changes; any fork (an unarmed child) or any unarmed member reverts it to the polling pace at once (the
//! watcher watches each member for its `NOTE_FORK`/`NOTE_EXIT`). [`ProcessBox::usage`] reads the box live,
//! so its accounting is current however long the watcher waits. The cap is per process, doesn't survive a fork, and caps each member at the
//! *whole* bound, not its share, so it doesn't bound a multi-member box's footprint sum — the watcher's
//! rule still does that (step 2's privileged helper apportions a per-member share). A member killed at
//! its cap dies by SIGKILL; the watcher tells that from an ordinary kill by the dead member's peak
//! footprint (`proc_pid_rusage`) having reached the cap, and counts it as a memory end.
//!
//! Rootless: a box is its process group. A member that daemonizes (setsid) leaves the group; the watcher
//! counts it (`Status::escaped`) and no longer holds it, which the next step's box-user closes. A box's
//! leader pid names the group, so a program that outlives the leader and whose leader pid is recycled to a
//! new group leader could be mislisted; this step documents that gap, which the box-user also closes.

use std::io;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Mutex, OnceLock};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};
use triomphe::Arc;

use hex_api::{Bounds, BoxId, Refusal, Signal, Usage};
use hex_core::record::{NativeRecord, OwnerKey, OwnerRef};
use hex_core::watch::{self, Life, Limit, Member, Rates, Verdict, Watch};

use super::sys::{self, Clamp, Event, Kqueue, Timebase};

/// Why a box ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Ended {
    /// The watcher found it past a bound and killed it.
    Over(watch::Over),
    /// It was deleted.
    Deleted,
}

/// A box as it is now.
#[derive(Debug, Clone, Default)]
pub struct Status {
    /// Its members still running (not a zombie awaiting reap).
    pub running: u64,
    /// Its processes, as its process bound counts them.
    pub processes: u64,
    /// Its members' footprint, summed, in bytes.
    pub footprint: u64,
    /// The highest footprint the watcher read.
    pub peak: u64,
    /// Times the watcher ended the box for its memory bound.
    pub memory_kills: u64,
    /// Times the watcher ended the box for its process bound.
    pub process_kills: u64,
    /// CPU its members used, in Mach ticks summed.
    pub cpu_ticks: u64,
    pub read_bytes: u64,
    pub written_bytes: u64,
    /// Members that left the box's process group by daemonizing (setsid): counted, no longer held (the
    /// next step's box-user closes this), never dropped silently (CLAUDE.md §3).
    pub escaped: u64,
    /// Set once the box has ended, with why.
    pub ended: Option<Ended>,
    /// Reads the watcher has made of the box.
    pub reads: u64,
    /// Reads that also redid the box's membership work (its kqueue watches, the escape check): one per
    /// change in its running members, not one per read (observability: the watcher's work per read).
    pub membership_passes: u64,
    /// How the read that ended the box for a bound came about, once one has (observability: what an
    /// overshoot is made of).
    pub end_trace: Option<EndTrace>,
    /// Reads that held any of the box's members stopped across their wait ([`watch::Watch::runs`];
    /// observability: what the hold costs the box).
    pub holds: u64,
}

/// The read that ended a box for a bound, taken apart: the wait armed before it, how long after the read
/// before it it came, what it cost, the footprint at both, and how long the members took to end after it.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct EndTrace {
    /// The wait the watcher armed after the read before (its headroom at the rates).
    pub armed: Duration,
    /// From the read before's start to this read's start: the armed wait plus how late the wake came.
    pub gap: Duration,
    /// This read's own time: the group listed and each member's rusage read.
    pub read_cost: Duration,
    /// The members this read listed.
    pub members: u64,
    /// The footprint the read before found, and this one.
    pub footprint_before: u64,
    pub footprint_at_end: u64,
    /// From this read's decision to the read that found no member running; `None` until then.
    pub to_empty: Option<Duration>,
    /// Whether the read before held any of the box's members stopped across its wait.
    pub held: bool,
}

/// What every box on the node shares, read once at node start: the machine's timebase and the allocation
/// rate the bound is reckoned against.
#[derive(Debug, Clone)]
pub struct Shared {
    tb: Timebase,
    /// The fastest a box is taken to grow, as this node learns it: seeded by the caller (its machine class's
    /// record, hex_core::watch::seed), raised by any faster growth a watcher sees (watch::LearnedRates).
    rates: Arc<Mutex<watch::LearnedRates>>,
    /// How long a killed group gets to empty, as this node learns it: the record's to start, raised by every
    /// slower drain any box sees (hex_core::drain::Learned; the learned-waits rule, 2026-11-06).
    drain: Arc<Mutex<hex_core::drain::Learned>>,
    /// How late the node's watchers act past their armed waits (a wake, its read and its stops), as it learns
    /// it: seeded by the caller (its machine class's record, hex_core::watch::lateness_seed), raised by any
    /// later act a watcher sees (watch::Lateness). What each box's hold reckons a member running across a
    /// wait can run on for.
    lateness: Arc<Mutex<watch::Lateness>>,
}

impl Shared {
    /// The node's shared timing. `rate_bytes` and `rate_processes` seed the fastest a box is taken to grow:
    /// the node's machine class's record (hex_core::watch::seed), which its watchers then raise as they see
    /// faster growth; `lateness` seeds how late a watcher's wake is taken to come (hex_core::watch::
    /// lateness_seed), raised the same way.
    pub fn new(
        rate_bytes: std::num::NonZeroU64,
        rate_processes: std::num::NonZeroU64,
        lateness: Duration,
    ) -> io::Result<Self> {
        Ok(Self {
            tb: Timebase::get()?,
            rates: Arc::try_new(Mutex::new(watch::LearnedRates::new(Rates {
                bytes: rate_bytes,
                processes: rate_processes,
            })))
            .map_err(|_| io::Error::from(io::ErrorKind::OutOfMemory))?,
            drain: Arc::try_new(Mutex::new(hex_core::drain::Learned::default()))
                .map_err(|_| io::Error::from(io::ErrorKind::OutOfMemory))?,
            lateness: Arc::try_new(Mutex::new(watch::Lateness::new(lateness)))
                .map_err(|_| io::Error::from(io::ErrorKind::OutOfMemory))?,
        })
    }

    /// The latest watcher wake past its wait the node has seen (its record's to start).
    pub fn lateness(&self) -> Result<Duration, Refusal> {
        Ok(self
            .lateness
            .lock()
            .map_err(|_| poisoned("the node's wake lateness"))?
            .worst())
    }

    /// The rates as learned so far.
    pub fn rates(&self) -> Result<Rates, Refusal> {
        Ok(self
            .rates
            .lock()
            .map_err(|_| poisoned("the node's growth rates"))?
            .rates())
    }
}

/// The state one box shares between the node's threads and its one watcher thread.
#[derive(Debug)]
struct State {
    id: OwnerKey,
    clamp: Clamp,
    /// The bounds the box was made with, to report on inspect; the watcher holds the [`Limit`] of them it
    /// enforces. Neither changes after create (a new bound is a new box).
    bounds: Bounds,
    /// What the box may hold, read once by its watcher.
    limit: Limit,
    /// The whole-bound fatal cap in bytes, when the box has a memory bound the SPI can take: the watcher
    /// reads a dead armed member's peak footprint against it to tell a cap kill from an ordinary exit
    /// (native plan step 1's `_ext` backstop). `None` leaves the box to the watcher's own rule.
    cap_bytes: Option<u64>,
    /// The box's process group id: the pid of its leader, the first program [`ProcessBox::exec`] spawns.
    pgid: Mutex<Option<i32>>,
    /// The leader's start time (`proc_pid_rusage`), read when it was spawned: with the group id, what names
    /// the box's members after its node is gone (native plan step 8, T-T7.1.2).
    leader_started: Mutex<Option<u64>>,
    status: Mutex<Status>,
    /// The pids the box armed with the fatal per-process cap — members it spawned, each kernel-bounded.
    /// The watcher reads it to leave a fully-armed one-member box to its events and to attribute a
    /// member's death at its cap to memory; a pid is dropped once gone or accounted, so the set stays
    /// bounded by the box's live member count (CLAUDE.md §2).
    armed: Mutex<Pids>,
    /// Whether the watcher is ending the box by its own rule (it issued a killpg for a bound). A member's
    /// waiter reads it so a SIGKILL the watcher sent isn't double-counted as a kernel cap kill — the
    /// watcher's End verdict already counted that bound. The watcher mirrors its own `ending` here.
    ending: AtomicBool,
    /// Set by [`ProcessBox::delete`] to end the box; the watcher kills it and stops.
    stop: AtomicBool,
    /// The members the watcher holds stopped (SIGSTOP) across its wait ([`watch::Watch::runs`]; the lead,
    /// 2026-11-08 and 2026-11-09). It continues what it held and only that, so a member the box's own
    /// programs stopped stays stopped.
    held: Mutex<Hold>,
    /// The watcher waits on it; the node triggers it to make a read happen at once (a member started, or
    /// the box is stopped).
    kq: Kqueue,
    vm: Option<super::managed::Scope>,
}

impl State {
    fn uncertain(&self) -> Refusal {
        if let Some(scope) = &self.vm {
            return scope.refusal(self.id.as_ref());
        }
        match &self.id {
            OwnerKey::Box(id) => super::managed::uncertain(id),
            OwnerKey::Pool(_) => super::native_allocation(std::mem::size_of::<State>()),
        }
    }
    fn validate_vm_scope(&self) -> Result<(), Refusal> {
        if let Some(scope) = &self.vm {
            scope.validate(
                self.pgid
                    .lock()
                    .map_err(|_| poisoned("a box's group"))?
                    .as_ref()
                    .copied(),
                self.leader_started
                    .lock()
                    .map_err(|_| poisoned("a box's leader"))?
                    .as_ref()
                    .copied(),
                self.id.as_ref(),
            )?;
        }
        Ok(())
    }
    fn status(&self) -> Result<std::sync::MutexGuard<'_, Status>, Refusal> {
        self.status.lock().map_err(|_| poisoned("a box's status"))
    }

    /// Wakes the watcher so it reads now rather than at its next timer. A box with no bound to pace by has
    /// no timer, so a wake that fails is the caller's refusal: the watcher wouldn't see the change.
    fn wake(&self) -> Result<(), Refusal> {
        self.kq
            .trigger()
            .map_err(|e| Refusal::os("waking a box's watcher", e))
    }

    fn held(&self) -> Result<std::sync::MutexGuard<'_, Hold>, Refusal> {
        self.held.lock().map_err(|_| poisoned("a box's hold"))
    }

    /// Stops member `pid`, just started beside another, and holds it: the watcher's next read decides whether
    /// it runs (the lead, 2026-11-08: that read can come late, and a member left running meanwhile grows
    /// unread).
    fn hold_new(&self, pid: i32) -> Result<(), Refusal> {
        let mut held = self.held()?;
        let additional = usize::from(!held.stopped.contains(&pid));
        held.stopped.reserve(additional)?;
        if let Some(member) = self.stopped_member(pid, &[])?
            && let Some(member) = self.signal_stopped(member, sys::MemberSignal::Stop)?
        {
            held.stopped.insert(member)?;
        }
        Ok(())
    }

    /// Continues one held member for its watcher's read, the `turn`th of those still in group `pgid` (one gone
    /// from it is no longer the watcher's to continue: its pid may be another process's), with its use before it
    /// ran, whole as it was stopped. `None` when none is held. It stays held: the watcher stops it again after
    /// the read, first, so nothing runs behind its stop.
    fn run_one(
        &self,
        pgid: i32,
        pids: &mut Vec<i32>,
        turn: usize,
    ) -> Result<Option<ReadRun>, Refusal> {
        let mut held = self.held()?;
        if held.stopped.is_empty() {
            return Ok(None);
        }
        sys::group(pgid, pids).map_err(|e| Refusal::os("listing a box's group", e))?;
        held.stopped.retain(|pid| pids.contains(pid));
        let Some(member) = turn
            .checked_rem(held.stopped.len())
            .and_then(|at| held.stopped.at(at))
        else {
            return Ok(None);
        };
        let pid = member.pid;
        // Gone since the listing (a pid no longer a process, or a managed member's pid another birth's): not
        // held, and nothing to run. A managed member's token never reaches another process.
        let Some(before) =
            sys::rusage(pid).map_err(|e| Refusal::os("reading a member's rusage", e))?
        else {
            held.stopped.remove(&pid);
            return Ok(None);
        };
        if member
            .native
            .is_some_and(|token| token.birth() != before.started)
        {
            held.stopped.remove(&pid);
            return Ok(None);
        }
        let Some(continued) = self.signal_stopped(member, sys::MemberSignal::Continue)? else {
            held.stopped.remove(&pid);
            return Ok(None);
        };
        held.stopped.refresh(continued)?;
        Ok(Some(ReadRun {
            pid,
            user: before.user,
            footprint: before.footprint,
        }))
    }

    /// Stops member `pid` again after the read it ran for; it stays held. One the caller's own stop or continue
    /// let go of since ([`ProcessBox::signal`]) is the box's own, left as it is.
    fn stop_again(&self, pid: i32) -> Result<(), Refusal> {
        let mut held = self.held()?;
        if let Some(member) = held.stopped.get(&pid) {
            match self.signal_stopped(member, sys::MemberSignal::Stop)? {
                Some(stopped) => held.stopped.refresh(stopped)?,
                None => {
                    held.stopped.remove(&pid);
                }
            }
        }
        Ok(())
    }

    /// Holds the box as its watcher's read decided (the graded hold, the lead 2026-11-09): of `order`, its
    /// running members, each in `run` runs across the wait, continued if the watcher held it, and the rest are
    /// held, each stopped in turn. A member the box's own programs stopped is theirs, never held, so never
    /// continued. Returns how many members it holds.
    fn apply(&self, order: &[u32], run: &[u32], members: &[Member]) -> Result<usize, Refusal> {
        let mut held = self.held()?;
        let additional = order
            .iter()
            .filter_map(|member| i32::try_from(*member).ok())
            .filter(|pid| !held.stopped.contains(pid))
            .count();
        held.stopped.reserve(additional)?;
        held.added.clear();
        held.continued.clear();
        let result = (|| {
            for &member in order {
                let Ok(pid) = i32::try_from(member) else {
                    continue;
                };
                if run.contains(&member) {
                    if let Some(member) = held.stopped.get(&pid) {
                        self.signal_stopped(member, sys::MemberSignal::Continue)?;
                        held.continued.push(pid);
                    }
                } else if !held.stopped.contains(&pid)
                    && sys::run_state(pid)
                        .map_err(|error| Refusal::os("reading a member's run state", error))?
                        == Some(sys::Run::Live)
                    && let Some(member) = self.stopped_member(pid, members)?
                    && let Some(member) = self.signal_stopped(member, sys::MemberSignal::Stop)?
                {
                    held.added.push_reserved(member)?;
                }
            }
            Ok::<(), Refusal>(())
        })();
        held.continued.sort_unstable();
        // Both work buffers and the final set were reserved before any CONT/STOP above.
        let Hold {
            stopped,
            added,
            continued,
        } = &mut *held;
        stopped.retain(|pid| continued.binary_search(pid).is_err());
        stopped.append_reserved(added)?;
        result?;
        Ok(held.stopped.len())
    }

    /// Continues every member the watcher holds that is still in group `pgid`, and holds none: what a watcher
    /// that stops watching leaves.
    fn release_all(&self, pgid: i32, pids: &mut Vec<i32>) -> Result<(), Refusal> {
        let mut held = self.held()?;
        sys::group(pgid, pids).map_err(|e| Refusal::os("listing a box's group", e))?;
        for at in 0..held.stopped.len() {
            let member = held.stopped.at(at).ok_or_else(|| self.uncertain())?;
            if pids.contains(&member.pid) {
                self.signal_stopped(member, sys::MemberSignal::Continue)?;
            }
        }
        held.stopped.clear();
        Ok(())
    }

    /// Member `pid` with its token, to be stopped; `None` for one gone since the read that listed it.
    fn stopped_member(&self, pid: i32, members: &[Member]) -> Result<Option<Stopped>, Refusal> {
        let Some(scope) = &self.vm else {
            return Ok(Some(Stopped { pid, native: None }));
        };
        self.validate_vm_scope()?;
        let birth = members
            .iter()
            .find(|member| i32::try_from(member.pid) == Ok(pid))
            .map(|member| member.started)
            .ok_or_else(|| self.uncertain())?;
        let group = self
            .pgid
            .lock()
            .map_err(|_| self.uncertain())?
            .ok_or_else(|| self.uncertain())?;
        if !self.in_scope(scope, pid, birth, group)? {
            return Ok(None);
        }
        let Some(token) = self.capture(scope, pid, birth)? else {
            return Ok(None);
        };
        if !self.in_scope(scope, pid, birth, group)? {
            return Ok(None);
        }
        Ok(Some(Stopped {
            pid,
            native: Some(token),
        }))
    }

    /// Member `pid`'s token, or `None` once it has gone: capture's own birth checks (Changed), no task
    /// (NoRight: XNU's task_name_for_pid finds the process but no task once it is exiting), or any other
    /// failure after which it is gone. A member alive with its birth that can't be captured is the scope's
    /// control failure.
    fn capture(
        &self,
        scope: &super::managed::Scope,
        pid: i32,
        birth: u64,
    ) -> Result<Option<sys::MemberToken>, Refusal> {
        match sys::capture_member(pid, birth) {
            Ok(token) => Ok(Some(token)),
            Err(sys::TokenFailure::Changed { .. } | sys::TokenFailure::NoRight) => Ok(None),
            Err(failure) => {
                if gone(pid, birth)? {
                    Ok(None)
                } else {
                    Err(scope.control_failed(failure, self.id.as_ref()))
                }
            }
        }
    }

    /// Signals `member`: in a managed scope by its audit token, else by pid. Returns the member as signalled,
    /// with the token that reached it, or `None` for one gone ([`gone`]: ended, reaped, or its pid another
    /// process's; an ordinary box's pid gone), which no signal is owed and no failure of the scope: members
    /// end on their own, and a scope's end races them (a controller exits once its foreground child is
    /// killed). A zombie or a reaped member is in no process group (`getpgid` finds no process), so the group
    /// check alone would take it for a process outside the scope. XNU gives a process a new audit-token pid
    /// version at exec, so a member that exec'd since its hold captured its token is refused by that token
    /// (ESRCH) though it is the same process: its pid and birth (its start, which exec keeps) are joined again
    /// in the scope's group, its token captured afresh, and the signal sent once with it.
    fn signal_stopped(
        &self,
        member: Stopped,
        signal: sys::MemberSignal,
    ) -> Result<Option<Stopped>, Refusal> {
        self.signal_stopped_by(member, signal, sys::signal_member)
    }

    /// [`Self::signal_stopped`], a managed member signalled by its token through `send`, the kernel's
    /// (`sys::signal_member`): a test ends the member inside it, between the scope's check and the signal, the
    /// race no test can stop inside otherwise.
    fn signal_stopped_by(
        &self,
        member: Stopped,
        signal: sys::MemberSignal,
        mut send: impl FnMut(&sys::MemberToken, sys::MemberSignal) -> Result<(), sys::TokenFailure>,
    ) -> Result<Option<Stopped>, Refusal> {
        if let Some(scope) = &self.vm {
            self.validate_vm_scope()?;
            let token = member.native.ok_or_else(|| self.uncertain())?;
            let group = self
                .pgid
                .lock()
                .map_err(|_| self.uncertain())?
                .ok_or_else(|| self.uncertain())?;
            if token.pid() != member.pid {
                scope.failed(shards_api::ProcessFailure::DifferentScope);
                return Err(self.uncertain());
            }
            if !self.in_scope(scope, member.pid, token.birth(), group)? {
                return Ok(None);
            }
            match send(&token, signal) {
                Ok(()) => return Ok(Some(member)),
                Err(failure) if !failure.unresolved() => {
                    return Err(scope.control_failed(failure, self.id.as_ref()));
                }
                Err(_) => {}
            }
            // Refused by its token: it ended since the check above, or it exec'd.
            let Some(fresh) = self.capture(scope, member.pid, token.birth())? else {
                return Ok(None);
            };
            if !self.in_scope(scope, member.pid, token.birth(), group)? {
                return Ok(None);
            }
            return match send(&fresh, signal) {
                Ok(()) => Ok(Some(Stopped {
                    pid: member.pid,
                    native: Some(fresh),
                })),
                // Refused by its fresh token too, and gone from the scope: it ended after the check above (an
                // exiting process's token resolves to none), the end the signal wanted, never a failure of the
                // scope (AC-T7.4.1).
                Err(failure)
                    if failure.unresolved()
                        && !self.in_scope(scope, member.pid, token.birth(), group)? =>
                {
                    Ok(None)
                }
                Err(failure) => Err(scope.control_failed(failure, self.id.as_ref())),
            };
        }
        let signal = match signal {
            sys::MemberSignal::Stop => rustix::process::Signal::STOP,
            sys::MemberSignal::Continue => rustix::process::Signal::CONT,
            sys::MemberSignal::Kill => rustix::process::Signal::KILL,
        };
        signal_one(member.pid, signal).map(|sent| if sent { Some(member) } else { None })
    }

    /// Whether member `pid`, born at `birth`, is still in the scope: `false` once it has gone ([`gone`]) or
    /// getpgid finds no process (it is exiting: XNU's proc_find leaves out an exiting process before its
    /// rusage says it ended); the scope's failure (DifferentScope) for one alive in another group, which
    /// has left the scope.
    fn in_scope(
        &self,
        scope: &super::managed::Scope,
        pid: i32,
        birth: u64,
        group: i32,
    ) -> Result<bool, Refusal> {
        if gone(pid, birth)? {
            return Ok(false);
        }
        match sys::group_of(pid)
            .map_err(|error| Refusal::os("checking owned member signal scope", error))?
        {
            Some(found) if found == group => Ok(true),
            None => Ok(false),
            // A pid reused since the read above is another process's: the member is gone.
            Some(_) if gone(pid, birth)? => Ok(false),
            Some(_) => {
                scope.failed(shards_api::ProcessFailure::DifferentScope);
                Err(self.uncertain())
            }
        }
    }

    /// Counts member `pid`'s death as a memory end — once — if the box armed it with the fatal cap, isn't
    /// being deleted, and isn't being ended by the watcher's own rule (whose End verdict counts that
    /// bound), and only when its `peak` (its zombie's lifetime peak, read before it was reaped) reached the
    /// cap: the kernel's fatal cap kills a member at it, so a `SIGKILL` of a member under it was another
    /// hand's (AppleSystemPolicy's refusal of an exec kills it so before it runs, CLAUDE.md §3: never a
    /// memory kill it wasn't). Dropping `pid` from the armed set makes the count idempotent across the
    /// member's waiter (on reap) and the watcher (on the exit event): whoever brings its peak first decides.
    /// A call with no peak decides nothing and leaves `pid` armed: the watcher reads an exit event's member
    /// when it wakes, which can be after the waiter reaped it, and the waiter read the zombie's peak before
    /// that reap (native plan step 1's `_ext` backstop; HEX-MAC-1, the figure HEX-RT-5's memory case and the
    /// overshoot bench read).
    fn killed_by_cap(&self, pid: i32, peak: Option<u64>) -> Result<(), Refusal> {
        if self.stop.load(Ordering::SeqCst) || self.ending.load(Ordering::SeqCst) {
            return Ok(());
        }
        let (Some(cap), Some(peak)) = (self.cap_bytes, peak) else {
            return Ok(());
        };
        if !self
            .armed
            .lock()
            .map_err(|_| poisoned("a box's armed set"))?
            .remove(&pid)
        {
            // Not an armed member, or already decided: the armed-set removal is the dedup point.
            return Ok(());
        }
        if peak < cap {
            return Ok(());
        }
        let bound = self.limit.memory.unwrap_or(peak);
        let mut status = self.status()?;
        status.memory_kills = status.memory_kills.saturating_add(1);
        status.peak = status.peak.max(peak);
        if status.ended.is_none() {
            status.ended = Some(Ended::Over(watch::Over::Memory {
                footprint: peak,
                bound,
            }));
        }
        Ok(())
    }
}

/// What the watcher holds of a box. Bounded by its members (CLAUDE.md §2): one gone from the box is dropped
/// at the next read that runs a held member.
#[derive(Debug)]
struct Hold {
    /// The members the watcher stopped, each its to continue; one the box's own programs stopped is never
    /// among them.
    stopped: Stops,
    added: Stops,
    continued: Vec<i32>,
}

#[derive(Debug, Clone, Copy)]
struct Stopped {
    pid: i32,
    native: Option<sys::MemberToken>,
}
#[derive(Debug)]
enum StopStorage {
    Plain(Vec<i32>),
    Native(Vec<Stopped>),
}
#[derive(Debug)]
struct Stops {
    values: StopStorage,
    bound: usize,
}
impl Stops {
    fn new(bound: usize, native: bool) -> Result<Self, Refusal> {
        Ok(Self {
            values: if native {
                StopStorage::Native(storage(bound)?)
            } else {
                StopStorage::Plain(storage(bound)?)
            },
            bound,
        })
    }
    fn search(&self, pid: &i32) -> Result<usize, usize> {
        match &self.values {
            StopStorage::Plain(values) => values.binary_search(pid),
            StopStorage::Native(values) => values.binary_search_by_key(pid, |member| member.pid),
        }
    }
    fn at(&self, at: usize) -> Option<Stopped> {
        match &self.values {
            StopStorage::Plain(values) => values.get(at).map(|pid| Stopped {
                pid: *pid,
                native: None,
            }),
            StopStorage::Native(values) => values.get(at).copied(),
        }
    }
    fn get(&self, pid: &i32) -> Option<Stopped> {
        self.search(pid).ok().and_then(|at| self.at(at))
    }
    fn contains(&self, pid: &i32) -> bool {
        self.get(pid).is_some()
    }
    fn remove(&mut self, pid: &i32) -> bool {
        let Ok(at) = self.search(pid) else {
            return false;
        };
        match &mut self.values {
            StopStorage::Plain(values) => {
                values.remove(at);
            }
            StopStorage::Native(values) => {
                values.remove(at);
            }
        }
        true
    }
    fn reserve(&mut self, additional: usize) -> Result<(), Refusal> {
        self.len()
            .checked_add(additional)
            .filter(|count| *count <= self.bound)
            .ok_or_else(|| super::native_allocation(self.bound))?;
        match &mut self.values {
            StopStorage::Plain(values) => values.try_reserve(additional),
            StopStorage::Native(values) => values.try_reserve(additional),
        }
        .map_err(|_| super::native_allocation(self.bound))
    }
    fn insert(&mut self, member: Stopped) -> Result<(), Refusal> {
        match self.search(&member.pid) {
            Ok(at) if self.at(at).is_some_and(|old| old.native != member.native) => {
                return Err(super::managed::configuration(
                    "held native incarnation cannot be replaced",
                ));
            }
            Err(at) => {
                self.reserve(1)?;
                match &mut self.values {
                    StopStorage::Plain(values) if member.native.is_none() => {
                        values.insert(at, member.pid)
                    }
                    StopStorage::Native(values) if member.native.is_some() => {
                        values.insert(at, member)
                    }
                    _ => {
                        return Err(super::managed::configuration(
                            "hold storage does not match native owner",
                        ));
                    }
                }
            }
            _ => {}
        }
        Ok(())
    }
    /// The same held member with the token its signal last reached it by ([`State::signal_stopped`], after an
    /// exec): only for the pid and birth held; [`Stops::insert`] never replaces one.
    fn refresh(&mut self, member: Stopped) -> Result<(), Refusal> {
        let StopStorage::Native(values) = &mut self.values else {
            return Ok(());
        };
        let held = values
            .binary_search_by_key(&member.pid, |held| held.pid)
            .ok()
            .and_then(|at| values.get_mut(at))
            .filter(|held| {
                held.native.map(|token| token.birth()) == member.native.map(|token| token.birth())
            })
            .ok_or_else(|| {
                super::managed::configuration("held native incarnation cannot be replaced")
            })?;
        *held = member;
        Ok(())
    }
    fn push_reserved(&mut self, member: Stopped) -> Result<(), Refusal> {
        if self.len() >= self.bound {
            return Err(super::native_allocation(self.bound));
        }
        match &mut self.values {
            StopStorage::Plain(values)
                if member.native.is_none() && values.len() < values.capacity() =>
            {
                values.push(member.pid)
            }
            StopStorage::Native(values)
                if member.native.is_some() && values.len() < values.capacity() =>
            {
                values.push(member)
            }
            _ => return Err(super::native_allocation(self.bound)),
        }
        Ok(())
    }
    fn append_reserved(&mut self, added: &Self) -> Result<(), Refusal> {
        for at in 0..added.len() {
            self.push_reserved(
                added
                    .at(at)
                    .ok_or_else(|| super::native_allocation(self.bound))?,
            )?;
        }
        match &mut self.values {
            StopStorage::Plain(values) => {
                values.sort_unstable();
                values.dedup();
            }
            StopStorage::Native(values) => {
                values.sort_unstable_by_key(|member| member.pid);
                values.dedup_by_key(|member| member.pid);
            }
        }
        Ok(())
    }
    fn retain(&mut self, mut keep: impl FnMut(&i32) -> bool) {
        match &mut self.values {
            StopStorage::Plain(values) => values.retain(keep),
            StopStorage::Native(values) => values.retain(|member| keep(&member.pid)),
        }
    }
    fn clear(&mut self) {
        match &mut self.values {
            StopStorage::Plain(values) => values.clear(),
            StopStorage::Native(values) => values.clear(),
        }
    }
    fn len(&self) -> usize {
        match &self.values {
            StopStorage::Plain(values) => values.len(),
            StopStorage::Native(values) => values.len(),
        }
    }
    fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

/// Sorted native members, bounded by this kernel's maximum process count. Whole snapshots use
/// sort once; an individual native start reserves before its effect and inserts only once.
#[derive(Debug, Default)]
struct Pids {
    values: Vec<i32>,
    bound: usize,
}
impl Pids {
    fn new(bound: usize) -> Result<Self, Refusal> {
        Ok(Self {
            values: storage(bound)?,
            bound,
        })
    }
    fn contains(&self, pid: &i32) -> bool {
        self.values.binary_search(pid).is_ok()
    }
    fn remove(&mut self, pid: &i32) -> bool {
        match self.values.binary_search(pid) {
            Ok(at) => {
                self.values.remove(at);
                true
            }
            Err(_) => false,
        }
    }
    fn insert(&mut self, pid: i32) -> Result<(), Refusal> {
        if let Err(at) = self.values.binary_search(&pid) {
            self.reserve(1)?;
            self.values.insert(at, pid);
        }
        Ok(())
    }
    fn reserve(&mut self, additional: usize) -> Result<(), Refusal> {
        let count = self
            .values
            .len()
            .checked_add(additional)
            .filter(|count| *count <= self.bound)
            .ok_or_else(|| super::native_allocation(self.bound))?;
        self.values
            .try_reserve(count.saturating_sub(self.values.len()))
            .map_err(|_| super::native_allocation(self.bound))
    }
    fn retain(&mut self, keep: impl FnMut(&i32) -> bool) {
        self.values.retain(keep);
    }
}
impl Hold {
    fn new(bound: usize, native: bool) -> Result<Self, Refusal> {
        Ok(Self {
            stopped: Stops::new(bound, native)?,
            added: Stops::new(bound, native)?,
            continued: storage(bound)?,
        })
    }
}
fn storage<T>(count: usize) -> Result<Vec<T>, Refusal> {
    let mut values = Vec::new();
    values
        .try_reserve_exact(count)
        .map_err(|_| super::native_allocation(count))?;
    Ok(values)
}

/// A held member continued for its watcher's read, and its use before it ran.
struct ReadRun {
    pid: i32,
    user: u64,
    footprint: u64,
}

/// A handle to a box's shared state that its members' waiters hold, so a member killed at the fatal cap
/// is counted the instant its waiter reaps it — before the caller that waited reads the box's usage, not
/// only when the watcher next polls (native plan step 1's `_ext` backstop). Cloneable.
#[derive(Clone)]
pub struct Members(Arc<State>);

impl Members {
    pub(super) fn uncertain(&self) -> Refusal {
        self.0.uncertain()
    }
    /// Tells the box a member ended: when `sigkill` and its `peak` (read from its zombie before the reap)
    /// reached the cap, counts a memory end once ([`State::killed_by_cap`]). A clean exit, another signal,
    /// or a SIGKILL under the cap is not a memory end.
    pub fn ended(&self, pid: i32, sigkill: bool, peak: Option<u64>) -> Result<(), Refusal> {
        if sigkill {
            self.0.killed_by_cap(pid, peak)
        } else {
            Ok(())
        }
    }

    /// Member `pid` is about to be sent KILL by its node (`Proc::kill`): its death is not the kernel's cap,
    /// so it leaves the armed set, which is where a cap kill is counted once ([`State::killed_by_cap`]).
    pub fn killing(&self, pid: i32) -> Result<(), Refusal> {
        self.0
            .armed
            .lock()
            .map_err(|_| poisoned("a box's armed set"))?
            .remove(&pid);
        Ok(())
    }
}

/// A macOS `process` box. Dropping it ends the box and joins its watcher.
pub struct ProcessBox {
    state: Arc<State>,
    watcher: Mutex<Option<JoinHandle<()>>>,
    /// The node's learned drain bound, shared by every box ([`Shared`]).
    drain: Arc<Mutex<hex_core::drain::Learned>>,
    /// The machine's timebase, for a live read's CPU.
    tb: Timebase,
    pub(super) controller: Mutex<super::managed::Controller>,
    pub(super) identity: OnceLock<shards_api::Identity>,
    pub(super) scope_pids: Mutex<Vec<i32>>,
}

impl ProcessBox {
    /// A box with `bounds`, its members clamped by `weight` (a low weight runs the box at background
    /// priority), watched at the node's `rates`. Nothing runs in it until [`ProcessBox::exec`].
    pub fn create(id: BoxId, bounds: &Bounds, shared: &Shared) -> Result<Self, Refusal> {
        Self::made(id, bounds, shared, None)
    }

    /// A box taken back after its node was gone (native plan step 8, T-T7.1.2), its watcher on `group` (its
    /// group id and leader's start time, as [`ProcessBox::group`] named them) where that group is still the
    /// box's ([`owned`]); else empty, as a box nothing runs in yet. Its members aren't this node's children,
    /// so their waits are launchd's; the watcher lists, reads and ends them as it does its own.
    pub fn adopt(
        id: BoxId,
        bounds: &Bounds,
        shared: &Shared,
        group: Option<(i32, u64)>,
    ) -> Result<Self, Refusal> {
        let group = match group {
            Some((pgid, started)) if owned(pgid, started)? => Some((pgid, started)),
            _ => None,
        };
        Self::made(id, bounds, shared, group)
    }

    fn made(
        id: BoxId,
        bounds: &Bounds,
        shared: &Shared,
        group: Option<(i32, u64)>,
    ) -> Result<Self, Refusal> {
        let boxed = Self::prepared(OwnerKey::Box(id), bounds, shared, group, None)?;
        boxed.start_watcher(shared)?;
        Ok(boxed)
    }

    fn prepared(
        id: OwnerKey,
        bounds: &Bounds,
        shared: &Shared,
        group: Option<(i32, u64)>,
        vm: Option<super::managed::Scope>,
    ) -> Result<Self, Refusal> {
        let maximum = sys::max_processes()
            .map_err(|error| Refusal::os("sizing native member storage", error))?;
        let scope_pids = if vm.is_some() {
            storage(maximum)?
        } else {
            Vec::new()
        };
        let state = Arc::try_new(State {
            id,
            clamp: clamp(bounds),
            bounds: bounds.clone(),
            limit: limit(bounds),
            cap_bytes: bounds.memory_max.and_then(sys::jetsam_cap_bytes),
            pgid: Mutex::new(group.map(|(pgid, _)| pgid)),
            leader_started: Mutex::new(group.map(|(_, started)| started)),
            status: Mutex::new(Status::default()),
            armed: Mutex::new(Pids::new(maximum)?),
            ending: AtomicBool::new(false),
            stop: AtomicBool::new(false),
            held: Mutex::new(Hold::new(maximum, vm.is_some())?),
            kq: Kqueue::new().map_err(|error| Refusal::os("making a box's kqueue", error))?,
            vm,
        })
        .map_err(|_| super::native_allocation(std::mem::size_of::<State>()))?;
        Ok(Self {
            state,
            watcher: Mutex::new(None),
            drain: Arc::clone(&shared.drain),
            tb: shared.tb,
            controller: Mutex::new(super::managed::Controller::Unsubmitted),
            identity: OnceLock::new(),
            scope_pids: Mutex::new(scope_pids),
        })
    }

    fn start_watcher(&self, shared: &Shared) -> Result<(), Refusal> {
        let watcher = Watcher::new(Arc::clone(&self.state), shared.clone())
            .map_err(|error| Refusal::os("sizing a box's watcher", error))?
            .start()
            .map_err(|error| Refusal::os("starting a box's watcher", error))?;
        let mut held = self
            .watcher
            .lock()
            .map_err(|_| poisoned("a box's watcher"))?;
        if held.is_some() {
            return Err(self.state.uncertain());
        }
        *held = Some(watcher);
        Ok(())
    }

    pub(super) fn prepare_managed(
        record: NativeRecord<'_>,
        bounds: &Bounds,
        shared: &Shared,
        control: Option<(super::CpuControl, u64)>,
        baseline: Duration,
    ) -> Result<Arc<Self>, Refusal> {
        let vm = super::managed::Scope::new(record, control, baseline)?;
        let observer = super::proc::Prepared::new()?;
        let boxed = Arc::try_new(Self::prepared(
            record.owner().try_owned()?,
            bounds,
            shared,
            None,
            Some(vm),
        )?)
        .map_err(|_| super::native_allocation(std::mem::size_of::<Self>()))?;
        *boxed
            .controller
            .lock()
            .map_err(|_| poisoned("native controller preparation"))? =
            super::managed::Controller::Prepared(observer);
        // Passive SIGCHLD readiness closes NOTE_EXIT-before-SZOMB without another VMM waiter or
        // time probe. It only wakes the same watcher; terminal facts still check this owned child.
        boxed
            .state
            .kq
            .child_completions()
            .map_err(|error| Refusal::os("observing managed waitable child readiness", error))?;
        boxed.start_watcher(shared)?;
        Ok(boxed)
    }

    pub(super) fn scope(&self) -> Option<&super::managed::Scope> {
        self.state.vm.as_ref()
    }
    pub(super) fn is_managed(&self) -> bool {
        self.scope().is_some()
    }

    pub(super) fn held_exec(&self, spawn: &sys::Spawn) -> Result<i32, Refusal> {
        let mut pgid = self
            .state
            .pgid
            .lock()
            .map_err(|_| poisoned("a box's group"))?;
        if pgid.is_some() {
            return Err(self.state.uncertain());
        }
        let mut armed = self
            .state
            .armed
            .lock()
            .map_err(|_| poisoned("a box's armed set"))?;
        if spawn.jetsam_mib.is_some() {
            armed.reserve(1)?;
        }
        self.scope()
            .ok_or_else(|| self.state.uncertain())?
            .held(self.owner())?;
        let pid = sys::spawn_suspended(spawn, 0, self.state.clamp)
            .map_err(|error| Refusal::os("creating the held native VM controller", error))?;
        // Keep the actual submitted PID before any further fallible observation/publication.
        *pgid = Some(pid);
        if spawn.jetsam_mib.is_some() {
            armed.insert(pid)?;
        }
        if pid <= 0 {
            return Err(self.state.uncertain());
        }
        Ok(pid)
    }

    pub(super) fn not_submitted(&self) -> Result<(), Refusal> {
        if self
            .state
            .pgid
            .lock()
            .map_err(|_| poisoned("a box's group"))?
            .is_some()
        {
            return Err(self.state.uncertain());
        }
        self.scope()
            .ok_or_else(|| self.state.uncertain())?
            .not_submitted(self.owner())
    }

    pub(super) fn introduce_group(&self, pid: i32, started: u64) -> Result<(), Refusal> {
        if pid <= 0 || started == 0 {
            return Err(self.state.uncertain());
        }
        {
            let mut group = self
                .state
                .pgid
                .lock()
                .map_err(|_| poisoned("a box's group"))?;
            if group.is_some_and(|old| old != pid) {
                return Err(self.state.uncertain());
            }
            *group = Some(pid);
        }
        {
            let mut birth = self
                .state
                .leader_started
                .lock()
                .map_err(|_| poisoned("a box's leader"))?;
            if birth.is_some_and(|old| old != started) {
                return Err(self.state.uncertain());
            }
            *birth = Some(started);
        }
        if let Some(scope) = self.scope() {
            let token = sys::capture_member(pid, started)
                .map_err(|failure| scope.control_failed(failure, self.owner()))?;
            self.state.held()?.stopped.insert(Stopped {
                pid,
                native: Some(token),
            })?;
        }
        Ok(())
    }

    /// Joint plan step 14 (C4 on macOS): the managed scope's CPU rate becomes `rate`, and the watcher is woken to hold
    /// or continue its members by it.
    pub(super) fn set_managed_rate(&self, rate: u64) -> Result<(), Refusal> {
        self.scope()
            .ok_or_else(|| self.state.uncertain())?
            .set_cpu_rate(self.owner(), rate)?;
        self.state.wake()
    }

    pub(super) fn resume_managed(&self) -> Result<(), Refusal> {
        let (pid, _) = self.group()?.ok_or_else(|| self.state.uncertain())?;
        let decision = self
            .scope()
            .ok_or_else(|| self.state.uncertain())?
            .start_cpu(self.owner())?;
        self.scope()
            .ok_or_else(|| self.state.uncertain())?
            .running(self.owner())?;
        if matches!(decision, super::cpu::Decision::Hold(_)) {
            // The controller is already natively suspended. Earn the same startup debt before
            // any CONT; the existing watcher resumes it from its retained audit-token Hold entry.
            self.scope()
                .ok_or_else(|| self.state.uncertain())?
                .cpu_held(self.owner())?;
            return self.state.wake();
        }
        if decision == super::cpu::Decision::Prepared {
            return Err(self.state.uncertain());
        }
        let mut held = self.state.held()?;
        let member = held
            .stopped
            .get(&pid)
            .ok_or_else(|| self.state.uncertain())?;
        if self
            .state
            .signal_stopped(member, sys::MemberSignal::Continue)?
            .is_none()
        {
            return Err(self.state.uncertain());
        }
        held.stopped.remove(&pid);
        drop(held);
        self.state.wake()
    }

    pub(super) fn stop_managed(&self) -> Result<(), Refusal> {
        let valid = self.state.validate_vm_scope();
        self.state.stop.store(true, Ordering::SeqCst);
        let woken = self.state.wake();
        valid?;
        woken
    }

    pub(super) fn stop_managed_raw(&self) -> Result<(), shards_api::ProcessFailure> {
        use shards_api::{ProcessFailure, ProcessOperation};
        let pgid = *self
            .state
            .pgid
            .lock()
            .map_err(|_| ProcessFailure::Poisoned {
                what: "native scope group",
            })?;
        let started = *self
            .state
            .leader_started
            .lock()
            .map_err(|_| ProcessFailure::Poisoned {
                what: "native scope leader",
            })?;
        self.scope()
            .ok_or(ProcessFailure::InvalidStartRecord)?
            .validate_native(pgid, started)?;
        self.state.stop.store(true, Ordering::SeqCst);
        self.state
            .kq
            .trigger()
            .map_err(|error| super::proc::process_failure(ProcessOperation::StopScope, error))
    }

    pub(super) fn finalize_watcher(&self, end: shards_api::ManagedEnd) -> Result<(), Refusal> {
        self.scope()
            .ok_or_else(|| self.state.uncertain())?
            .finalizing(end, self.owner())?;
        self.state.stop.store(true, Ordering::SeqCst);
        self.state.wake()?;
        if let Some(watcher) = self
            .watcher
            .lock()
            .map_err(|_| poisoned("a box's watcher"))?
            .take()
        {
            watcher.join().map_err(|_| self.state.uncertain())?;
        }
        Ok(())
    }

    pub fn id(&self) -> Result<&BoxId, Refusal> {
        match &self.state.id {
            OwnerKey::Box(id) => Ok(id),
            OwnerKey::Pool(_) => Err(self.state.uncertain()),
        }
    }

    pub(super) fn owner(&self) -> OwnerRef<'_> {
        self.state.id.as_ref()
    }

    /// A handle for a member's waiter to tell the box the member ended, so a kill at the fatal cap is
    /// counted the instant the waiter reaps it (native plan step 1's `_ext` backstop).
    pub fn members(&self) -> Members {
        Members(Arc::clone(&self.state))
    }

    /// The bounds the box was made with.
    pub fn bounds(&self) -> Bounds {
        self.state.bounds.clone()
    }

    /// Spawns `program` into the box: the first into a new process group it leads, each later one into that
    /// group, so every member and everything it forks is in the box from its first instruction. Once every
    /// member has ended the group is gone and none can join it, so the next program leads a new one, as a
    /// box on any other OS takes a program after its last ended (HEX-DIFF-1). Returns its pid.
    pub fn exec(&self, spawn: sys::Spawn) -> Result<i32, Refusal> {
        if self.state.stop.load(Ordering::SeqCst) {
            return Err(Refusal::NoSuchBox(self.id()?.try_clone()?));
        }
        let mut pgid = self
            .state
            .pgid
            .lock()
            .map_err(|_| poisoned("a box's group"))?;
        let group = match *pgid {
            Some(g) if live(g)? => g,
            _ => 0,
        };
        let carries_cap = spawn.jetsam_mib.is_some();
        if carries_cap {
            self.state
                .armed
                .lock()
                .map_err(|_| poisoned("a box's armed set"))?
                .reserve(1)?;
        }
        let pid = sys::spawn(&spawn, group, self.state.clamp)
            .map_err(|e| Refusal::os("spawning a program in a box", e))?;
        if group == 0 {
            *pgid = Some(pid);
            // Read now, while the leader is the node's unreaped child, so its start time names it.
            let started = sys::rusage(pid)
                .map_err(|e| Refusal::os("reading a box's leader", e))?
                .map(|r| r.started);
            *self
                .state
                .leader_started
                .lock()
                .map_err(|_| poisoned("a box's leader"))? = started;
        }
        drop(pgid);
        // The member carries the fatal kernel cap: track it so the watcher paces by it and attributes a
        // death at the cap to memory (native plan step 1's `_ext` backstop).
        if carries_cap {
            self.state
                .armed
                .lock()
                .map_err(|_| poisoned("a box's armed set"))?
                .insert(pid)?;
        }
        // A member started beside another, whose sum no kernel cap bounds, waits stopped for the watcher's
        // next read, which continues it: that read can come late (the lead, 2026-11-08), and a member left
        // running meanwhile grows unread. A box's first member is its fatal cap's alone.
        if group != 0 && self.state.limit.memory.is_some() {
            self.state.hold_new(pid)?;
        }
        self.state.wake()?;
        Ok(pid)
    }

    /// The box's process group and its leader's start time, once its first member has started: what names
    /// its members after its node is gone, for the node to record (native plan step 8, T-T7.1.2).
    pub fn group(&self) -> Result<Option<(i32, u64)>, Refusal> {
        let pgid = *self
            .state
            .pgid
            .lock()
            .map_err(|_| poisoned("a box's group"))?;
        let started = *self
            .state
            .leader_started
            .lock()
            .map_err(|_| poisoned("a box's leader"))?;
        Ok(pgid.zip(started))
    }

    /// The box as it is now.
    pub fn status(&self) -> Result<Status, Refusal> {
        self.state.status().map(|s| s.clone())
    }

    /// Whether the box has a live member right now, read live (not from the watcher's last poll, which
    /// lags), as the Linux class reads its cgroup's population for inspect.
    pub fn running_now(&self) -> Result<bool, Refusal> {
        match *self
            .state
            .pgid
            .lock()
            .map_err(|_| poisoned("a box's group"))?
        {
            Some(pgid) => live(pgid),
            None => Ok(false),
        }
    }

    /// What the box has used and holds now, as a [`Usage`]: its members read live (the group listed, each
    /// one's rusage), as the Linux class reads its cgroup's files, so it is current however long the
    /// watcher waits (a box with no bound to pace by is read only when it changes). The kill counts and
    /// the peak are the watcher's, the peak raised to what this read finds.
    pub fn usage(&self) -> Result<Usage, Refusal> {
        let pgid = *self
            .state
            .pgid
            .lock()
            .map_err(|_| poisoned("a box's group"))?;
        let live = if self.is_managed() {
            Some(self.managed_live(pgid)?)
        } else {
            match pgid {
                Some(pgid) => Some(read_live(pgid)?),
                None => None,
            }
        };
        let mut s = self.state.status()?;
        if let Some(live) = live {
            s.footprint = live.footprint;
            s.processes = live.processes;
            s.peak = s.peak.max(live.footprint);
            s.cpu_ticks = self.tb.nanos(live.cpu) / 1000;
            s.read_bytes = s.read_bytes.max(live.read);
            s.written_bytes = s.written_bytes.max(live.written);
        }
        Ok(Usage {
            memory_bytes: s.footprint,
            memory_peak_bytes: s.peak,
            oom_kills: s.memory_kills,
            pids: s.processes,
            pids_refused: s.process_kills,
            // Mach ticks are reported as microseconds by [`sys::Timebase`] when the node reads usage; the
            // raw ticks are kept here, converted where the timebase is to hand.
            cpu_usec: s.cpu_ticks,
            io_read_bytes: s.read_bytes,
            io_write_bytes: s.written_bytes,
            guest: None,
        })
    }

    fn managed_live(&self, pgid: Option<i32>) -> Result<Live, Refusal> {
        self.state.validate_vm_scope()?;
        let mut live = Live {
            footprint: 0,
            processes: 0,
            cpu: 0,
            read: 0,
            written: 0,
        };
        let helper = managed_helper_usage()?;
        if let Some(pgid) = pgid {
            let mut pids = self.scope_pids.lock().map_err(|_| self.state.uncertain())?;
            if sys::group(pgid, &mut pids)
                .map_err(|error| Refusal::os("listing strict managed usage", error))?
            {
                return Err(self.state.uncertain());
            }
            let own = rustix::process::getpid().as_raw_nonzero().get();
            for &pid in pids.iter() {
                if pid == own {
                    return Err(self.state.uncertain());
                }
                let usage = sys::rusage(pid)
                    .map_err(|error| Refusal::os("reading strict managed usage", error))?
                    .ok_or_else(|| self.state.uncertain())?;
                live.add(usage, !(usage.ended && pid == pgid));
            }
        }
        // Whole-helper memory/CPU is conservatively charged to every active canonical allocation.
        // It is an actual native process observation, never a fake Member to stop or a new row.
        live.add(helper, true);
        Ok(live)
    }

    /// Sends `signal` to every member (killpg). KILL is the rule's own end; any other is passed on. Refused
    /// as not running when no member is alive, as every class refuses it (HEX-DIFF-2).
    pub fn signal(&self, signal: Signal) -> Result<(), Refusal> {
        let macos = hex_core::darwin::signal_to_macos(signal.number()).ok_or(Refusal::Invalid {
            what: "signal",
            why: format!("signal {} has no macOS number", signal.number()),
        })?;
        let not_running = || -> Result<Refusal, Refusal> {
            Ok(Refusal::NotRunning(self.id()?.try_clone()?, "signal"))
        };
        match *self
            .state
            .pgid
            .lock()
            .map_err(|_| poisoned("a box's group"))?
        {
            Some(pgid) if live(pgid)? => {
                // A stop or a continue the caller sends is the box's own: what the watcher held is no longer
                // its to continue.
                if [
                    libc::SIGSTOP,
                    libc::SIGTSTP,
                    libc::SIGTTIN,
                    libc::SIGTTOU,
                    libc::SIGCONT,
                ]
                .contains(&macos)
                {
                    self.state.held()?.stopped.clear();
                }
                if killpg(pgid, macos)? {
                    Ok(())
                } else {
                    Err(not_running()?)
                }
            }
            _ => Err(not_running()?),
        }
    }

    /// Ends the box: stops its watcher, kills every member (killpg KILL) and waits for them to die, leaving
    /// each member's own waiter to reap it. Idempotent.
    pub fn delete(&self) -> Result<(), Refusal> {
        if self.is_managed() {
            return Err(self.state.uncertain());
        }
        self.state.stop.store(true, Ordering::SeqCst);
        self.state.wake()?;
        if let Some(handle) = self
            .watcher
            .lock()
            .map_err(|_| poisoned("a box's watcher"))?
            .take()
        {
            let _ = handle.join();
        }
        // The watcher has killed the group and set `ended`; make sure no member still runs.
        if let Some(pgid) = *self
            .state
            .pgid
            .lock()
            .map_err(|_| poisoned("a box's group"))?
        {
            let mut pids = sys::listing().map_err(|e| Refusal::os("listing a box's group", e))?;
            // The group's size sets how long it gets to empty: the measured kill-to-empty envelope, doubled,
            // as this node has learned it (hex_core::drain). Past it the kill didn't take, a fault named
            // (CLAUDE.md §4); a delete has no other box to try, so the caller gets it.
            let members = alive_in(pgid, &mut pids)?;
            let patience = self
                .drain
                .lock()
                .map_err(|_| poisoned("the node's drain bound"))?
                .bound(members);
            let killed = Instant::now();
            let _ = killpg(
                pgid,
                hex_core::darwin::signal_to_macos(Signal::KILL.number()).unwrap_or(9),
            );
            let deadline = killed.checked_add(patience);
            loop {
                if alive_in(pgid, &mut pids)? == 0 {
                    if let Ok(mut learned) = self.drain.lock() {
                        learned.observe(members, killed.elapsed());
                    }
                    return Ok(());
                }
                if deadline.is_none_or(|d| Instant::now() >= d) {
                    return Err(Refusal::os(
                        "emptying a box",
                        io::Error::other(format!("box {} still holds a live member", self.id()?)),
                    ));
                }
                std::thread::yield_now();
            }
        }
        Ok(())
    }
}

impl Drop for ProcessBox {
    fn drop(&mut self) {
        if self.is_managed() {
            // A failed pre-effect construction can join its own watcher immediately. Submitted
            // native scopes retain their watcher/receipt until exact durable Released/finalize;
            // dropping a proxy or a runtime value is not a scope-empty witness.
            if let Some(scope) = self.scope()
                && self.group().is_ok_and(|group| group.is_none())
                && scope.no_submission(self.owner()).unwrap_or(false)
            {
                let _ = scope.finalizing(
                    shards_api::ManagedEnd {
                        status: shards_api::ManagedStatus::Unknown,
                        scope: shards_api::TerminalScope::Released,
                    },
                    self.owner(),
                );
            }
            let _ = self.stop_managed();
            let finalized = self.scope().is_some_and(|scope| scope.is_finalized());
            if let Ok(watcher) = self.watcher.get_mut()
                && let Some(watcher) = watcher.take()
                && finalized
            {
                let _ = watcher.join();
            }
        } else {
            let _ = self.delete();
        }
    }
}

/// A box's group as one live read finds it: its footprint and processes as the watcher's rule counts them,
/// and its members' summed CPU (Mach ticks) and I/O.
struct Live {
    footprint: u64,
    processes: u64,
    cpu: u64,
    read: u64,
    written: u64,
}

impl Live {
    fn add(&mut self, usage: sys::Rusage, count: bool) {
        // Values beyond u64 exceed every representable native resource bound.
        self.footprint = self.footprint.saturating_add(usage.footprint);
        self.processes = self.processes.saturating_add(u64::from(count));
        self.cpu = self.cpu.saturating_add(usage.cpu);
        self.read = self.read.saturating_add(usage.read_bytes);
        self.written = self.written.saturating_add(usage.written_bytes);
    }
}

fn managed_helper_usage() -> Result<sys::Rusage, Refusal> {
    let pid = rustix::process::getpid().as_raw_nonzero().get();
    sys::rusage(pid)
        .map_err(|error| Refusal::os("reading same native helper usage", error))?
        .filter(|usage| !usage.ended && usage.started != 0)
        .ok_or_else(|| super::managed::configuration("native helper usage has no live incarnation"))
}

/// Include the actual helper in the SAME allocation without making it a process-group member.
/// Ordinary Watch rules remain untouched. The helper cannot be bounded by a VMM member's individual
/// jetsam cap, so its combined headroom always uses the existing aggregate wait/hold formula.
fn managed_watch_read(
    read: &mut watch::Read,
    limit: &Limit,
    rates: &Rates,
    helper: u64,
    ending: bool,
) {
    read.footprint = read.footprint.saturating_add(helper);
    read.processes = read.processes.saturating_add(1);
    if read.running == 0 {
        return;
    }
    if matches!(read.verdict, Verdict::End(_) | Verdict::Kill) {
        return;
    }
    if ending {
        read.verdict = Verdict::Kill;
        return;
    }
    if let Some(bound) = limit.memory
        && read.footprint > bound
    {
        read.verdict = Verdict::End(watch::Over::Memory {
            footprint: read.footprint,
            bound,
        });
        return;
    }
    let next = watch::next(limit, rates, read.footprint, read.processes);
    read.verdict = match (read.verdict, next) {
        (Verdict::Wait(before), Some(after)) => Verdict::Wait(before.min(after)),
        (Verdict::Wait(before), None) => Verdict::Wait(before),
        (_, Some(after)) => Verdict::Wait(after),
        (before, None) => before,
    };
}

/// Reads process group `pgid` now: each listed member's rusage, a pid gone since the listing (ESRCH) left
/// out, and the leader's zombie not counted as a process while it is held for the group's id, as
/// [`watch::Watch::read`] counts.
fn read_live(pgid: i32) -> Result<Live, Refusal> {
    let mut pids = sys::listing().map_err(|e| Refusal::os("listing a box's group", e))?;
    sys::group(pgid, &mut pids).map_err(|e| Refusal::os("listing a box's group", e))?;
    let mut live = Live {
        footprint: 0,
        processes: 0,
        cpu: 0,
        read: 0,
        written: 0,
    };
    for &pid in &pids {
        let Some(r) = sys::rusage(pid).map_err(|e| Refusal::os("reading a member's rusage", e))?
        else {
            continue;
        };
        if !(r.ended && pid == pgid) {
            live.processes = live.processes.saturating_add(1);
        }
        live.footprint = live.footprint.saturating_add(r.footprint);
        live.cpu = live.cpu.saturating_add(r.cpu);
        live.read = live.read.saturating_add(r.read_bytes);
        live.written = live.written.saturating_add(r.written_bytes);
    }
    Ok(live)
}

/// Watches `pids` for their forks and ends on `kq`; returns those already ended. XNU's `EVFILT_PROC` reports
/// only what happens after it attaches (`filt_procattach`, `bsd/kern/kern_event.c`), and refuses a gone
/// process (`ESRCH`) but not a zombie: a member that ended between the listing and its watch posts no exit
/// event (c-route's finding on the node's socket, 2026-11-07).
#[cfg(test)]
pub(crate) fn watch_members(kq: &Kqueue, pids: &[i32]) -> Result<Vec<i32>, Refusal> {
    let mut ended = kq
        .watch(pids, 0)
        .map_err(|e| Refusal::os("watching a box's members", e))?;
    // Read each one again after the attach: one ended (a zombie) or gone by now posted its end before the
    // watch could see it. One that ends after this read posts its exit to the watch.
    for &pid in pids {
        if ended.contains(&pid) {
            continue;
        }
        match sys::rusage(pid).map_err(|e| Refusal::os("reading a member's rusage", e))? {
            Some(r) if !r.ended => {}
            _ => ended.push(pid),
        }
    }
    Ok(ended)
}

/// Whether process group `pgid` has a member alive (not a zombie).
fn live(pgid: i32) -> Result<bool, Refusal> {
    let mut pids = sys::listing().map_err(|e| Refusal::os("listing a box's group", e))?;
    Ok(alive_in(pgid, &mut pids)? > 0)
}

/// Whether process group `pgid`, its leader started at `started`, is still that box's group: its leader
/// still runs with that start time, or no process has the leader's pid now (it ended, and its group's other
/// members are left). A live process with that pid and another start time was given it since, and leads a
/// group of its own (the rootless gap the module's note states; box users close it).
pub fn owned(pgid: i32, started: u64) -> Result<bool, Refusal> {
    Ok(
        match sys::rusage(pgid).map_err(|e| Refusal::os("reading a box's leader", e))? {
            Some(r) => r.started == started,
            None => true,
        },
    )
}

/// Ends what runs of box group `pgid` (its leader started at `started`), for a box its node's recovery
/// removes: killpg KILL where the group is still the box's ([`owned`]), and its members waited out within
/// the measured kill-to-empty envelope for the group's size (`hex_core::drain::bound`; a recovering node
/// has learned none of its own yet). Past it the kill didn't take, a fault named (CLAUDE.md §4).
pub fn end_group(pgid: i32, started: u64) -> Result<(), Refusal> {
    if !owned(pgid, started)? {
        return Ok(());
    }
    let mut pids = sys::listing().map_err(|e| Refusal::os("listing a box's group", e))?;
    let members = alive_in(pgid, &mut pids)?;
    let _ = killpg(
        pgid,
        hex_core::darwin::signal_to_macos(Signal::KILL.number()).unwrap_or(9),
    );
    let deadline = Instant::now().checked_add(hex_core::drain::bound(members));
    loop {
        if alive_in(pgid, &mut pids)? == 0 {
            return Ok(());
        }
        if deadline.is_none_or(|d| Instant::now() >= d) {
            return Err(Refusal::os(
                "ending a box's group",
                io::Error::other(format!("group {pgid} still holds a live member")),
            ));
        }
        std::thread::yield_now();
    }
}

/// How many of process group `pgid`'s members are alive (not a zombie), by reading each one's rusage.
fn alive_in(pgid: i32, pids: &mut Vec<i32>) -> Result<usize, Refusal> {
    sys::group(pgid, pids).map_err(|e| Refusal::os("listing a box's group", e))?;
    let mut alive = 0usize;
    for pid in pids.iter() {
        match sys::rusage(*pid).map_err(|e| Refusal::os("reading a member's rusage", e))? {
            Some(r) if !r.ended => alive = alive.saturating_add(1),
            _ => {}
        }
    }
    Ok(alive)
}

/// The watcher of one box: a thread that reads the box every (bound − footprint) /
/// rate, holds its bounds, and ends it when it passes one.
struct Watcher {
    state: Arc<State>,
    shared: Shared,
    watch: Watch,
    /// The members seen at the last read, to find which went and which are new (escapes, [`watch::changes`]).
    last: Vec<Member>,
    /// A cheap signature of the last read's running members (summed pids and start times): the escape check
    /// — which allocates — runs only when it changes, so a box's steady members cost nothing between reads.
    last_sig: u64,
    /// The running members watched on the box's kqueue for their forks and ends, so a fork (an unarmed
    /// child) or a member's death wakes the watcher at once rather than at its next timer.
    watched: Vec<i32>,
    next_watched: Vec<i32>,
    present: Vec<i32>,
    previous_pids: Vec<i32>,
    current_keys: Vec<(u32, u64)>,
    maximum: usize,
    pids: Vec<i32>,
    /// The last read's listing, to tell whether this one changed.
    last_listing: Vec<i32>,
    events: Vec<Event>,
    /// When the last read was and what it found (footprint, processes): the growth to the next read is what
    /// the node's rates learn from.
    last_read: Option<(Instant, u64, u64)>,
    /// The last read's start, the wait it armed, the footprint it found and whether it held the box across
    /// that wait: the end trace's "before".
    before: Option<(Instant, Duration, u64, bool)>,
    /// When the watcher decided to end the box, until the read that finds it empty.
    ending_since: Option<Instant>,
    /// When the last wait was armed, and for how long: how late the watcher acted past it (its wake, its read
    /// and its stops) is what the node's holds reckon with.
    armed_at: Option<(Instant, Duration)>,
    /// The members' user times at the last read, by `last`'s order: a member whose user time rose since had a
    /// core in its run.
    last_user: Vec<u64>,
    /// When the last read's hold was in place: the members it let run have run across the wait since.
    ran_from: Option<Instant>,
    /// Advances each read: whose turn it is to run first, among the box's members, and among the held ones,
    /// to run for the read.
    turn: usize,
    /// The running members in this read's turn, what each member's run showed, and which run across the wait:
    /// kept between reads so a read allocates nothing for them.
    order: Vec<u32>,
    ran: Vec<watch::Run>,
    run: Vec<u32>,
}

impl Watcher {
    fn new(state: Arc<State>, shared: Shared) -> io::Result<Self> {
        let rates = shared
            .rates()
            .map_err(|e| io::Error::other(e.to_string()))?;
        let maximum = sys::max_processes()?;
        let mut watch = Watch::new(state.limit, rates);
        watch.reserve_members(maximum).map_err(io::Error::from)?;
        let reserved = |error: Refusal| io::Error::other(error.to_string());
        Ok(Self {
            state,
            shared,
            watch,
            last: storage(maximum).map_err(reserved)?,
            last_sig: 0,
            watched: storage(maximum).map_err(reserved)?,
            next_watched: storage(maximum).map_err(reserved)?,
            present: storage(maximum).map_err(reserved)?,
            previous_pids: storage(maximum).map_err(reserved)?,
            current_keys: storage(maximum).map_err(reserved)?,
            maximum,
            pids: sys::listing_from(Ok(maximum))?,
            events: storage(sys::EVENTS).map_err(reserved)?,
            last_listing: storage(maximum).map_err(reserved)?,
            last_read: None,
            before: None,
            ending_since: None,
            armed_at: None,
            last_user: storage(maximum).map_err(reserved)?,
            ran_from: None,
            turn: 0,
            order: storage(maximum).map_err(reserved)?,
            ran: storage(maximum.checked_add(1).ok_or(io::ErrorKind::OutOfMemory)?)
                .map_err(reserved)?,
            run: storage(maximum).map_err(reserved)?,
        })
    }

    fn start(mut self) -> io::Result<JoinHandle<()>> {
        let (prefix, suffix) = match self.state.id.as_ref() {
            OwnerRef::Box(id) => ("hex-box-", id.as_str().len()),
            OwnerRef::Pool(pool) => (
                "hex-pool-",
                pool.len()
                    .checked_mul(2)
                    .ok_or(io::ErrorKind::OutOfMemory)?,
            ),
        };
        let count = prefix
            .len()
            .checked_add(suffix)
            .ok_or(io::ErrorKind::OutOfMemory)?;
        let mut name = String::new();
        name.try_reserve_exact(count).map_err(io::Error::from)?;
        name.push_str(prefix);
        match self.state.id.as_ref() {
            OwnerRef::Box(id) => name.push_str(id.as_str()),
            OwnerRef::Pool(pool) => {
                use std::fmt::Write;
                for byte in pool {
                    write!(name, "{byte:02x}").map_err(|_| io::ErrorKind::OutOfMemory)?;
                }
            }
        }
        std::thread::Builder::new()
            .name(name)
            .spawn(move || self.run())
    }

    fn run(&mut self) {
        // A plain timeshare thread: the watcher holds a box's bound by reading it, never by taking priority
        // over other programs (Ada's rule, 2026-11-03; the lead, 2026-11-06).
        loop {
            if self.state.stop.load(Ordering::SeqCst) {
                if let Some(scope) = &self.state.vm {
                    if scope.is_finalized() {
                        return;
                    }
                    let state = Arc::clone(&self.state);
                    if state.validate_vm_scope().is_ok() {
                        self.finish();
                    }
                    if let Some(scope) = &state.vm {
                        let _ = scope.notify();
                    }
                    // Keep the SAME watcher alive for remaining helpers' exit/readiness and an
                    // explicit later Stop after a transient cleanup failure. No timer/poll is used.
                    if let Err(error) = state.kq.wait(None, None, &mut self.events) {
                        if let Some(scope) = &state.vm {
                            scope.failed(super::proc::process_failure(
                                shards_api::ProcessOperation::KeventExit,
                                error,
                            ));
                            let _ = scope.notify();
                        }
                        return;
                    }
                    continue;
                }
                if self
                    .state
                    .vm
                    .as_ref()
                    .is_none_or(|scope| !scope.is_finalized())
                    && self.state.validate_vm_scope().is_ok()
                {
                    self.finish();
                }
                return;
            }
            // Account any member the kernel killed at its fatal cap, from the exit events the last wait
            // delivered, before this read prunes the armed set of pids now gone.
            if self.account_exit_events().is_err() {
                let native = Arc::clone(&self.state);
                if let Some(scope) = &native.vm {
                    self.finish();
                    scope.failed(shards_api::ProcessFailure::InvalidStartRecord);
                }
                return;
            }
            let next = match self.step() {
                Ok(next) => next,
                // A read failed (a lock poisoned, a call errored): stop watching rather than spin. The box
                // is left as it is for delete to clean up, its held members continued, since no read of this
                // watcher will continue them.
                Err(_) => {
                    let state = Arc::clone(&self.state);
                    if let Some(scope) = &state.vm {
                        if state.validate_vm_scope().is_ok() {
                            self.finish();
                        }
                        scope.failed(shards_api::ProcessFailure::Poisoned {
                            what: "native scope watcher",
                        });
                    } else if let Ok(Some(pgid)) = self.state.pgid.lock().map(|group| *group) {
                        let _ = self.state.release_all(pgid, &mut self.pids);
                    }
                    return;
                }
            };
            // A timer only where a bound needs one; otherwise the box's next change wakes it: a member's
            // fork or end, a start (exec) or a delete, each a kqueue event.
            self.armed_at = next.map(|wait| (Instant::now(), wait));
            if let Err(error) = self.state.kq.wait(next, None, &mut self.events) {
                let state = Arc::clone(&self.state);
                if let Some(scope) = &state.vm {
                    if state.validate_vm_scope().is_ok() {
                        self.finish();
                    }
                    scope.failed(super::proc::process_failure(
                        shards_api::ProcessOperation::KeventExit,
                        error,
                    ));
                    return;
                }
            }
        }
    }

    /// One read and its decision; returns the wait before the next read, or `None` to wait until the box
    /// changes (it is idle).
    fn step(&mut self) -> Result<Option<Duration>, Refusal> {
        if let Some(scope) = &self.state.vm
            && !scope.watchable(self.state.id.as_ref())?
        {
            return Ok(None);
        }
        self.state.validate_vm_scope()?;
        let Some(pgid) = *self
            .state
            .pgid
            .lock()
            .map_err(|_| poisoned("a box's group"))?
        else {
            return Ok(None);
        };
        let native = Arc::clone(&self.state);
        let cpu = native
            .vm
            .as_ref()
            .map(|scope| scope.cpu_decision(native.id.as_ref()))
            .transpose()?
            .flatten();
        let cpu_run = match cpu {
            None => true,
            Some(super::cpu::Decision::Run(_)) => {
                native
                    .vm
                    .as_ref()
                    .ok_or_else(|| native.uncertain())?
                    .cpu_permit(native.id.as_ref())?;
                true
            }
            Some(super::cpu::Decision::Hold(_)) => false,
            Some(super::cpu::Decision::Prepared) => return Err(native.uncertain()),
        };
        let mut reading = Reading {
            members: storage(self.maximum)?,
            users: storage(self.maximum)?,
            totals: Totals::default(),
            sig: 0,
        };
        let armed = self.armed_at.take();
        self.turn = self.turn.wrapping_add(1);
        // One held member runs for this read, each in turn, so its own rate shows: the read's only run, it is
        // stopped again first, so nothing runs behind its stop (XNU's stop waits on each running member's core
        // before the next: the hold's stops compared, the lead 2026-11-09).
        let read_run = if cpu_run {
            self.state.run_one(pgid, &mut self.pids, self.turn)?
        } else {
            None
        };
        let ran_since = Instant::now();
        let rates = self.shared.rates()?;
        self.watch.rated(rates);
        let started = Instant::now();
        self.read_members(pgid, &mut reading)?;
        self.ran.clear();
        if let Some(before) = read_run {
            // Whether a thread of it ran or waited for a core while it was continued: with none it was asleep,
            // which shows what it does.
            let runnable = runnable(before.pid);
            self.state.stop_again(before.pid)?;
            let over = ran_since.elapsed();
            // Stopped again, its use is whole: this read's footprint of it, and the user time it ran.
            if let Some(after) =
                sys::rusage(before.pid).map_err(|e| Refusal::os("reading a member's rusage", e))?
            {
                let pid = u32::try_from(before.pid).unwrap_or(0);
                if let Some((m, user)) = reading
                    .members
                    .iter_mut()
                    .zip(reading.users.iter_mut())
                    .find(|(m, _)| m.pid == pid)
                {
                    m.footprint = after.footprint;
                    *user = after.user;
                }
                self.ran.push(watch::Run {
                    pid,
                    over,
                    across: false,
                    shown: after.user > before.user
                        || after.footprint > before.footprint
                        || !runnable,
                });
            }
        }
        let read_cost = started.elapsed();
        // The membership work (the escape check, which diffs the member lists and allocates, and the
        // kqueue rewatch) runs only when the running set changed: a box with steady members costs a listing
        // and a rusage a member per read, nothing more (the lead, 2026-11-06: less work per read).
        let membership_changed = reading.sig != self.last_sig;
        if membership_changed {
            self.count_escapes(&reading.members)?;
            self.last_sig = reading.sig;
        }
        // Each member that ran across the wait since the last read's hold: its run is the time since. One the hold
        // kept stopped ran only for this read (above), and one new since has no read before it to grow from.
        if let Some(from) = self.ran_from {
            let over = from.elapsed();
            let held = self.state.held()?;
            for (m, user) in reading.members.iter().zip(&reading.users) {
                let pid = i32::try_from(m.pid).unwrap_or(-1);
                if m.life != Life::Running || held.stopped.contains(&pid) {
                    continue;
                }
                let Some((last, last_user)) = self
                    .last
                    .iter()
                    .zip(&self.last_user)
                    .find(|(l, _)| l.pid == m.pid && l.started == m.started)
                else {
                    continue;
                };
                self.ran.push(watch::Run {
                    pid: m.pid,
                    over,
                    across: true,
                    shown: m.footprint > last.footprint || *user > *last_user || !runnable(pid),
                });
            }
        }
        let mut read = self
            .watch
            .read(&reading.members)
            .map_err(|_| Refusal::Exhausted {
                what: "watcher growth snapshot",
                bound: reading.members.len(),
            })?;
        if self.state.vm.is_some() {
            let helper = managed_helper_usage()?;
            managed_watch_read(
                &mut read,
                &self.state.limit,
                &rates,
                helper.footprint,
                self.state.ending.load(Ordering::SeqCst),
            );
            reading.totals.cpu = reading.totals.cpu.saturating_add(helper.cpu);
            reading.totals.read = reading.totals.read.saturating_add(helper.read_bytes);
            reading.totals.written = reading.totals.written.saturating_add(helper.written_bytes);
        }
        self.watch.ran(&self.ran);
        // The growth since the last read: a box faster than the node's rates raises them.
        let now = Instant::now();
        if let Some((then, footprint, processes)) = self.last_read
            && let Ok(mut learned) = self.shared.rates.lock()
        {
            learned.observe(
                read.footprint.saturating_sub(footprint),
                read.processes.saturating_sub(processes),
                now.saturating_duration_since(then),
            );
        }
        self.last_read = Some((now, read.footprint, read.processes));
        self.record(&read, &reading.totals)?;
        // Mirror the watcher's own ending for a member's waiter: a SIGKILL the watcher sent isn't a
        // separate kernel cap kill to count.
        self.state.ending.store(
            self.watch.ending().is_some()
                || matches!(read.verdict, Verdict::End(_) | Verdict::Kill),
            Ordering::SeqCst,
        );
        // Watch the current members for their forks and ends, so the next membership change wakes at once.
        if membership_changed {
            self.rewatch(&reading.members)?;
        }
        let members = u64::try_from(reading.members.len()).unwrap_or(u64::MAX);
        self.last = reading.members;
        self.last_user = reading.users;
        if let Verdict::End(_) = read.verdict {
            let (armed, gap, footprint_before, held) = self.before.map_or(
                (Duration::ZERO, Duration::ZERO, 0, false),
                |(at, armed, fp, held)| (armed, started.saturating_duration_since(at), fp, held),
            );
            self.state.status()?.end_trace = Some(EndTrace {
                armed,
                gap,
                read_cost,
                members,
                footprint_before,
                footprint_at_end: read.footprint,
                to_empty: None,
                held,
            });
            self.ending_since = Some(started);
        }
        if read.verdict == Verdict::Idle
            && let Some(since) = self.ending_since.take()
            && let Some(trace) = self.state.status()?.end_trace.as_mut()
        {
            trace.to_empty = Some(since.elapsed());
        }
        let next = match read.verdict {
            Verdict::Wait(d) => Some(d),
            Verdict::End(_) | Verdict::Kill => {
                if self.state.vm.is_some() {
                    self.end_managed_scope()?;
                } else {
                    let _ = killpg(pgid, kill_signal());
                }
                Some(watch::FLOOR)
            }
            Verdict::Idle | Verdict::Events => None,
        };
        // The graded hold (the lead, 2026-11-09): the members whose own rates over the wait and the lateness fit
        // the box's headroom run across the wait, each in turn; the rest are held stopped, and a box none of whose
        // members fits is read again at the floor.
        self.order.clear();
        self.order.extend(
            self.last
                .iter()
                .filter(|m| m.life == Life::Running)
                .map(|m| m.pid),
        );
        let first = self.turn.checked_rem(self.order.len()).unwrap_or(0);
        self.order.rotate_left(first);
        let floor = self
            .watch
            .runs(&read, &self.order, self.shared.lateness()?, &mut self.run)
            .map_err(|_| Refusal::Exhausted {
                what: "watcher run selection",
                bound: self.order.len(),
            })?;
        if !cpu_run {
            self.run.clear();
        }
        let mut held = self.state.apply(&self.order, &self.run, &self.last)?;
        self.ran_from = Some(Instant::now());
        let next = floor.or(next);
        let next = self.cpu_pace(native.vm.as_ref(), next, &mut held)?;
        // How late the watcher acted past its wait: its wake, its read and its stops, through all of which a
        // member running across the wait runs on (the lead, 2026-11-09: from the reads' own observed delays).
        if let Some((at, wait)) = armed {
            let late = at.elapsed().saturating_sub(wait);
            self.shared
                .lateness
                .lock()
                .map_err(|_| poisoned("the node's wake lateness"))?
                .observe(late);
        }
        if held > 0 {
            let mut status = self.state.status()?;
            status.holds = status.holds.saturating_add(1);
        }
        self.before = Some((
            started,
            next.unwrap_or(Duration::ZERO),
            read.footprint,
            held > 0,
        ));
        Ok(next)
    }

    fn cpu_pace(
        &mut self,
        scope: Option<&super::managed::Scope>,
        next: Option<Duration>,
        held: &mut usize,
    ) -> Result<Option<Duration>, Refusal> {
        let Some(scope) = scope else { return Ok(next) };
        let owner = self.state.id.as_ref();
        let decision = scope.cpu_decision(owner)?;
        let wait = match decision {
            None => return Ok(next),
            Some(super::cpu::Decision::Prepared) => return Err(self.state.uncertain()),
            Some(super::cpu::Decision::Run(wait)) => wait,
            Some(super::cpu::Decision::Hold(_)) => {
                *held = self.state.apply(&self.order, &[], &self.last)?;
                scope.cpu_held(owner)?;
                match scope.cpu_decision(owner)? {
                    Some(super::cpu::Decision::Hold(wait)) => Some(wait),
                    Some(super::cpu::Decision::Run(_)) => {
                        // Credit earned during this read/stop permits the next observed turn. A
                        // notification, not a periodic probe or an invented minimum CPU interval.
                        self.state.wake()?;
                        None
                    }
                    _ => return Err(self.state.uncertain()),
                }
            }
        };
        Ok(match (next, wait) {
            (Some(memory), Some(cpu)) => Some(memory.min(cpu)),
            (memory, None) => memory,
            (None, cpu) => cpu,
        })
    }

    /// The box's members now and their summed CPU and I/O: each listed pid with its footprint, start time,
    /// life and whether the box armed it, read through its rusage, once per member. A pid the listing
    /// holds but rusage no longer has (ESRCH) was reaped since: left out. Prunes the armed set of pids
    /// gone, so it stays bounded by the live members (CLAUDE.md §2).
    fn read_members(&mut self, pgid: i32, reading: &mut Reading) -> Result<(), Refusal> {
        if sys::group(pgid, &mut self.pids).map_err(|e| Refusal::os("listing a box's group", e))? {
            return Err(super::native_allocation(self.maximum));
        }
        // The armed set's pruning needs the listing's pids as sets: built only when the listing changed.
        let listing_changed = self.pids != self.last_listing;
        self.previous_pids.clear();
        if listing_changed {
            self.previous_pids.extend(
                self.last
                    .iter()
                    .filter_map(|member| i32::try_from(member.pid).ok()),
            );
            self.previous_pids.sort_unstable();
        }
        let mut armed = self
            .state
            .armed
            .lock()
            .map_err(|_| poisoned("a box's armed set"))?;
        let members = &mut reading.members;
        let users = &mut reading.users;
        self.present.clear();
        let mut totals = Totals::default();
        let mut sig = 0u64;
        for &pid in &self.pids {
            let Some(r) =
                sys::rusage(pid).map_err(|e| Refusal::os("reading a member's rusage", e))?
            else {
                continue;
            };
            if listing_changed {
                self.present.push(pid);
            }
            // The leader's zombie, while other members run, is the node holding the pid so the group id
            // can't be recycled: not the box's own process. Any other ended member is counted until reaped,
            // as Linux's `pids.max` counts a task until it is reaped.
            let life = match (r.ended, pid == pgid) {
                (true, true) => Life::Held,
                (true, false) => Life::Ended,
                (false, _) => Life::Running,
            };
            totals.cpu = totals.cpu.saturating_add(r.cpu);
            totals.read = totals.read.saturating_add(r.read_bytes);
            totals.written = totals.written.saturating_add(r.written_bytes);
            if life == Life::Running {
                // Order-independent so the listing's order doesn't matter; a member and its start time, so a
                // recycled pid shifts it. The multiplier is Fibonacci hashing's 2^32/phi (Knuth, TAOCP vol. 3,
                // §6.4), which spreads consecutive pids across the word.
                sig = sig.wrapping_add(
                    (u64::try_from(pid).unwrap_or(0))
                        .wrapping_mul(0x9e37_79b9)
                        .wrapping_add(r.started),
                );
            }
            members.push(Member {
                pid: u32::try_from(pid).unwrap_or(0),
                started: r.started,
                footprint: r.footprint,
                life,
                armed: armed.contains(&pid),
            });
            users.push(r.user);
        }
        // Prune armed pids that were listed before and are gone now (reaped): keeps the set bounded by the
        // live members, without dropping a just-spawned pid the listing hasn't caught up with yet.
        if listing_changed {
            self.present.sort_unstable();
            armed.retain(|pid| {
                self.present.binary_search(pid).is_ok()
                    || self.previous_pids.binary_search(pid).is_err()
            });
            self.last_listing.clear();
            self.last_listing.extend_from_slice(&self.pids);
        }
        drop(armed);
        reading.totals = totals;
        reading.sig = sig;
        Ok(())
    }

    /// Counts the members that left the group alive since the last read: each daemonized (setsid), which
    /// the box no longer holds (the next step's box-user does). A member whose rusage is gone or ended just
    /// exited, and isn't an escape.
    fn count_escapes(&mut self, now: &[Member]) -> Result<(), Refusal> {
        self.current_keys.clear();
        self.current_keys
            .extend(now.iter().map(|member| (member.pid, member.started)));
        self.current_keys.sort_unstable();
        let mut escaped = 0u64;
        for member in self
            .last
            .iter()
            .filter(|member| member.life == Life::Running)
        {
            if self
                .current_keys
                .binary_search(&(member.pid, member.started))
                .is_ok()
            {
                continue;
            }
            let pid = i32::try_from(member.pid).map_err(|_| self.state.uncertain())?;
            match sys::rusage(pid)
                .map_err(|error| Refusal::os("reading a gone member's rusage", error))?
            {
                Some(actual) if actual.started == member.started && !actual.ended => {
                    escaped = escaped.saturating_add(1)
                }
                _ => {}
            }
        }
        if escaped > 0 {
            let mut status = self.state.status()?;
            status.escaped = status.escaped.saturating_add(escaped);
        }
        Ok(())
    }

    /// Writes what the read found and decided into the box's status.
    fn record(&self, read: &watch::Read, totals: &Totals) -> Result<(), Refusal> {
        let mut status = self.state.status()?;
        status.running = read.running;
        status.processes = read.processes;
        status.footprint = read.footprint;
        // Max, not assignment: a member the kernel killed at its cap recorded its peak in account_jetsam
        // before this read, and the watcher's own peak (summed live footprints) can be lower — a box read
        // only on its events may never have seen the member near its cap while it was alive.
        status.peak = status.peak.max(self.watch.peak()).max(read.footprint);
        status.reads = self.watch.reads();
        // In microseconds, from the machine's timebase (`cpu` is Mach ticks).
        status.cpu_ticks = self.shared.tb.nanos(totals.cpu) / 1000;
        // Monotonic: a member gone since last read no longer reports its I/O, so keep the most seen.
        status.read_bytes = status.read_bytes.max(totals.read);
        status.written_bytes = status.written_bytes.max(totals.written);
        if let Verdict::End(over) = read.verdict {
            match over {
                watch::Over::Memory { .. } => {
                    status.memory_kills = status.memory_kills.saturating_add(1)
                }
                watch::Over::Processes { .. } => {
                    status.process_kills = status.process_kills.saturating_add(1)
                }
            }
            status.ended = Some(Ended::Over(over));
        }
        Ok(())
    }

    /// Accounts the box's members the kernel killed at their fatal cap, from the exit events the last wait
    /// delivered — the path for a member no waiter reaps in time (e.g. the overshoot bench holds its
    /// [`BoxProc`]s and polls). The node is the member's parent, so the kill signal rides in the event
    /// (`NOTE_EXITSTATUS`): a `SIGKILL` of a box member is the kernel's cap or memory pressure.
    /// [`State::killed_by_cap`] counts it once (guarding a delete or the watcher's own kill) with the
    /// zombie's lifetime peak while it is still readable. The member's own waiter accounts the same kill
    /// on reap, deterministically, so a caller that waited sees the count; whichever comes first wins.
    fn account_exit_events(&self) -> Result<(), Refusal> {
        for e in &self.events {
            if let Event::Exit { pid, status, .. } = *e
                && (status & 0x7f) == kill_signal()
            {
                // Its zombie's lifetime peak, while its waiter hasn't reaped it: a pid gone, or already
                // another process's, gives none, and `killed_by_cap` leaves the decision to the waiter,
                // which read the peak before its reap. It guards the box being deleted or the watcher's own
                // kill, and counts each death once.
                let peak = sys::rusage(pid)
                    .ok()
                    .flatten()
                    .filter(|r| r.ended)
                    .map(|r| r.peak);
                self.state.killed_by_cap(pid, peak)?;
            }
        }
        if self.events.iter().any(|event| {
            matches!(
                event,
                Event::Exit { .. } | Event::Signal { .. } | Event::Fork { exited: true, .. }
            )
        }) && let Some(scope) = &self.state.vm
        {
            scope.notify().map_err(|_| self.state.uncertain())?;
        }
        Ok(())
    }

    /// Watches the box's running members for their forks and ends on the box's kqueue, so a fork (an
    /// unarmed child) or a death wakes the watcher at once — to put an events-paced box back on headroom, or
    /// to read a cap kill's peak before the member's own waiter reaps it — rather than at its next timer.
    /// Members gone since the last read are unwatched (the kernel already dropped an exited member's knote,
    /// so that unwatch is a no-op it tolerates).
    fn rewatch(&mut self, members: &[Member]) -> Result<(), Refusal> {
        {
            let mut status = self.state.status()?;
            status.membership_passes = status.membership_passes.saturating_add(1);
        }
        self.next_watched.clear();
        self.next_watched.extend(
            members
                .iter()
                .filter(|member| member.life == Life::Running)
                .filter_map(|member| i32::try_from(member.pid).ok()),
        );
        self.next_watched.sort_unstable();
        self.next_watched.dedup();
        for &pid in &self.next_watched {
            if self.watched.binary_search(&pid).is_ok() {
                continue;
            }
            let child = self
                .state
                .armed
                .lock()
                .map_err(|_| poisoned("a box's armed set"))?
                .contains(&pid);
            let gone = self
                .state
                .kq
                .watch_one(pid, 0, child)
                .map_err(|error| Refusal::os("watching a box's member", error))?;
            if gone
                || sys::rusage(pid)
                    .map_err(|error| Refusal::os("reading a watched member's rusage", error))?
                    .is_none_or(|usage| usage.ended)
            {
                self.state.wake()?;
            }
        }
        for &pid in &self.watched {
            if self.next_watched.binary_search(&pid).is_err() {
                self.state
                    .kq
                    .unwatch(pid)
                    .map_err(|error| Refusal::os("unwatching a box's member", error))?;
            }
        }
        std::mem::swap(&mut self.watched, &mut self.next_watched);
        Ok(())
    }

    /// On stop: end the box. Kill the group and mark it deleted if the rule hadn't already ended it.
    fn finish(&mut self) {
        let native = Arc::clone(&self.state);
        if let Some(scope) = &native.vm {
            if self.end_managed_scope().is_err() {
                scope.failed(shards_api::ProcessFailure::InvalidStartRecord);
            }
            return;
        }
        if let Ok(Some(pgid)) = self.state.pgid.lock().map(|g| *g) {
            let _ = killpg(pgid, kill_signal());
        }
        if let Ok(mut status) = self.state.status.lock()
            && status.ended.is_none()
        {
            status.ended = Some(Ended::Deleted);
        }
    }

    fn end_managed_scope(&mut self) -> Result<(), Refusal> {
        self.state.validate_vm_scope()?;
        let scope = self
            .state
            .vm
            .as_ref()
            .ok_or_else(|| self.state.uncertain())?;
        let group = *self.state.pgid.lock().map_err(|_| self.state.uncertain())?;
        let Some(pgid) = group else {
            return if scope.no_submission(self.state.id.as_ref())? {
                Ok(())
            } else {
                Err(self.state.uncertain())
            };
        };
        if sys::group(pgid, &mut self.pids)
            .map_err(|error| Refusal::os("listing owned scope to stop", error))?
        {
            return Err(self.state.uncertain());
        }
        for &pid in &self.pids {
            // A member gone since the listing (ended or reaped: one the scope's own end let finish, as a
            // controller whose child was killed) needs no end.
            let Some(actual) = sys::rusage(pid)
                .map_err(|error| Refusal::os("reading owned scope to stop", error))?
            else {
                continue;
            };
            if actual.ended {
                continue;
            }
            let Some(token) = self.state.capture(scope, pid, actual.started)? else {
                continue;
            };
            self.state.signal_stopped(
                Stopped {
                    pid,
                    native: Some(token),
                },
                sys::MemberSignal::Kill,
            )?;
        }
        let mut status = self.state.status()?;
        if status.ended.is_none() {
            status.ended = Some(Ended::Deleted);
        }
        Ok(())
    }
}

/// A read's summed CPU (Mach ticks) and I/O over the box's members.
#[derive(Debug, Clone, Copy, Default)]
struct Totals {
    cpu: u64,
    read: u64,
    written: u64,
}

/// What one read of a box found: its members (each with its life and whether the box armed it), the
/// summed CPU/IO totals, and a signature of the running set (for the escape check).
struct Reading {
    members: Vec<Member>,
    /// Each member's user time, by `members`' order.
    users: Vec<u64>,
    totals: Totals,
    sig: u64,
}

/// What a box's bounds cap, as the watcher's rule reads them: its memory, and its process count (a thread
/// or a process both count on macOS, as `pids.max` counts both on Linux).
fn limit(bounds: &Bounds) -> Limit {
    Limit {
        memory: bounds.memory_max,
        processes: bounds.pids_max.map(u64::from),
    }
}

/// A box's QoS clamp, from its declared CPU weight as `cpu.weight` sets a box's share on Linux: background
/// where its weight is under the default 100, none otherwise (the lead, 2026-11-06: no clamp to keep the
/// watcher ahead of a box).
fn clamp(bounds: &Bounds) -> Clamp {
    match bounds.cpu_weight {
        Some(w) if w < 100 => Clamp::Background,
        _ => Clamp::Unclamped,
    }
}

fn kill_signal() -> i32 {
    // KILL is 9 on both kernels; the table confirms it, falling back to 9 only to satisfy the type.
    hex_core::darwin::signal_to_macos(Signal::KILL.number()).unwrap_or(9)
}

fn poisoned(what: &str) -> Refusal {
    Refusal::Broken(format!("{what} lock is poisoned"))
}

/// Whether process `pid` has a thread running or waiting for a core. Unread (gone, or another user's program),
/// it may have: a run of it with no other sign shows nothing.
fn runnable(pid: i32) -> bool {
    sys::task(pid).map_or(true, |t| t.is_none_or(|t| t.runnable))
}

/// Signals process `pid`; false when it is gone (ESRCH).
/// Whether the managed member born at `birth` with `pid` is gone: no process has its pid (reaped), its
/// zombie has it (ended), or another process does (another birth).
fn gone(pid: i32, birth: u64) -> Result<bool, Refusal> {
    Ok(sys::rusage(pid)
        .map_err(|error| Refusal::os("reading a held member's end", error))?
        .is_none_or(|usage| usage.ended || usage.started != birth))
}

fn signal_one(pid: i32, signal: rustix::process::Signal) -> Result<bool, Refusal> {
    let p = rustix::process::Pid::from_raw(pid).ok_or(Refusal::Invalid {
        what: "pid",
        why: format!("{pid} isn't a process id"),
    })?;
    match rustix::process::kill_process(p, signal) {
        Ok(()) => Ok(true),
        Err(rustix::io::Errno::SRCH) => Ok(false),
        Err(e) => Err(Refusal::os("kill", e)),
    }
}

/// Signals process group `pgid`; false when the group has no member left (ESRCH).
fn killpg(pgid: i32, signal: i32) -> Result<bool, Refusal> {
    let sig = rustix::process::Signal::from_named_raw(signal).ok_or(Refusal::Invalid {
        what: "signal",
        why: format!("{signal} isn't a signal this kernel names"),
    })?;
    let pid = rustix::process::Pid::from_raw(pgid).ok_or(Refusal::Invalid {
        what: "pgid",
        why: format!("{pgid} isn't a process group"),
    })?;
    match rustix::process::kill_process_group(pid, sig) {
        Ok(()) => Ok(true),
        Err(rustix::io::Errno::SRCH) => Ok(false),
        Err(e) => Err(Refusal::os("killpg", e)),
    }
}

#[cfg(test)]
mod scope_completion_tests;

#[cfg(test)]
mod exec_hold_tests;

#[cfg(test)]
mod storage_tests {
    #[test]
    fn actual_helper_cost_uses_aggregate_headroom_without_becoming_a_member() {
        let rates = Rates {
            bytes: std::num::NonZeroU64::new(4096).unwrap(),
            processes: std::num::NonZeroU64::new(4).unwrap(),
        };
        let limit = Limit {
            memory: Some(8192),
            processes: None,
        };
        let mut armed = watch::Read {
            verdict: Verdict::Events,
            footprint: 4096,
            processes: 1,
            running: 1,
        };
        managed_watch_read(&mut armed, &limit, &rates, 4096, false);
        assert_eq!(
            (armed.footprint, armed.processes, armed.running),
            (8192, 2, 1)
        );
        assert_eq!(
            armed.verdict,
            watch::next(&limit, &rates, 8192, 2).map_or(Verdict::Events, Verdict::Wait)
        );
        let mut aggregate = watch::Read {
            footprint: 4097,
            ..armed
        };
        managed_watch_read(&mut aggregate, &limit, &rates, 4096, false);
        assert_eq!(
            aggregate.verdict,
            Verdict::End(watch::Over::Memory {
                footprint: 8193,
                bound: 8192
            })
        );
        let mut idle = watch::Read {
            verdict: Verdict::Idle,
            footprint: 0,
            processes: 0,
            running: 0,
        };
        managed_watch_read(&mut idle, &limit, &rates, 4096, false);
        assert_eq!(
            (idle.verdict, idle.footprint, idle.running),
            (Verdict::Idle, 4096, 0)
        );
        // Idle native VM members do not credit the still-running helper's actual charge.
        assert_eq!(idle.processes, 1);
    }

    use super::*;

    /// AC-T7.4.1/R01: a native member-storage refusal precedes mutation of existing membership.
    #[test]
    fn bounded_member_storage_preserves_the_existing_snapshot() {
        let mut pids = Pids::new(2).unwrap();
        pids.insert(9).unwrap();
        pids.insert(3).unwrap();
        pids.insert(3).unwrap();
        assert_eq!(pids.values, [3, 9]);
        assert!(pids.reserve(1).is_err());
        assert_eq!(pids.values, [3, 9]);
        assert!(pids.remove(&3));
        pids.insert(4).unwrap();
        assert_eq!(pids.values, [4, 9]);
        assert!(Pids::new(usize::MAX).is_err());
    }

    /// AC-T7.4.1: inserting a same-PID replacement (another birth) or a later exec-token never replaces a
    /// held entry. The original token/birth and the bounded Hold entries remain unchanged. A held token
    /// changes only by refresh, for the same pid and birth, after the kernel refused the old one
    /// (`State::signal_stopped`; exec_hold_tests); a refresh of another birth is refused.
    #[test]
    fn native_hold_retains_exact_incarnation_across_replacement_and_exhaustion() {
        let mut held = Stops::new(1, true).unwrap();
        let original = sys::simulated_member(89, 97, 101);
        held.insert(Stopped {
            pid: 89,
            native: Some(original),
        })
        .unwrap();
        for replacement in [
            sys::simulated_member(89, 103, 107),
            sys::simulated_member(89, 97, 109),
        ] {
            assert!(
                held.insert(Stopped {
                    pid: 89,
                    native: Some(replacement)
                })
                .is_err()
            );
            assert_eq!(held.get(&89).unwrap().native, Some(original));
        }
        assert!(held.reserve(1).is_err());
        assert_eq!(held.get(&89).unwrap().native, Some(original));
        assert_eq!(held.len(), 1);
        assert!(
            held.refresh(Stopped {
                pid: 89,
                native: Some(sys::simulated_member(89, 103, 107)),
            })
            .is_err()
        );
        assert_eq!(held.get(&89).unwrap().native, Some(original));
        let execd = sys::simulated_member(89, 97, 109);
        held.refresh(Stopped {
            pid: 89,
            native: Some(execd),
        })
        .unwrap();
        assert_eq!(held.get(&89).unwrap().native, Some(execd));
        assert_eq!(held.len(), 1);
    }

    /// AC-T7.4.1: ordinary Hold keeps its original PID-sized buffers and sorted stop ownership;
    /// the managed token payload is allocated only for an actual managed native scope.
    #[test]
    fn ordinary_hold_keeps_plain_bounded_member_storage() {
        let mut hold = Hold::new(2, false).unwrap();
        assert!(matches!(hold.stopped.values, StopStorage::Plain(_)));
        assert!(matches!(hold.added.values, StopStorage::Plain(_)));
        hold.stopped
            .insert(Stopped {
                pid: 17,
                native: None,
            })
            .unwrap();
        hold.added
            .push_reserved(Stopped {
                pid: 11,
                native: None,
            })
            .unwrap();
        hold.stopped.append_reserved(&hold.added).unwrap();
        assert_eq!(hold.stopped.at(0).unwrap().pid, 11);
        assert_eq!(hold.stopped.at(1).unwrap().pid, 17);
        assert!(hold.stopped.reserve(1).is_err());
        assert!(hold.stopped.contains(&11) && hold.stopped.contains(&17));
    }
}

#[cfg(feature = "native-trusted-entry-measurement")]
pub mod trusted_entry;
