//! The macOS box helper daemon (native plan step 2). A box runs as a user of its own from a pool the
//! installer makes; the privileged launchd daemon starts each box's programs as that user, arms their
//! physical-footprint ledger limits and catches their Mach exceptions, and ends a box by killing every
//! process of its user. The node drives it over the [`hex_core::helper`] protocol.
//!
//! This module is the daemon's membership and bookkeeping: the live members of a box's pool user and
//! their footprints (`proc_listpids(PROC_UID_ONLY)` through [`super::sys::user_pids`]), whether the user
//! is empty (delete frees the user back to the pool only once it is), and killing every process of the
//! user. A box's process count is the kernel's — `RLIMIT_NPROC` of its user, set when the leader starts.
//! The privileged spawn and the ledger/exception arming are [`super::sys`]'s (`unsafe`, CLAUDE.md §12);
//! the apportionment is [`hex_core::boxuser`].

use std::collections::{BTreeMap, BTreeSet};
use std::io;
use std::os::fd::{AsFd, BorrowedFd, OwnedFd};
use std::path::Path;
use std::sync::Mutex;
use std::time::Instant;

use rustix::net::{AddressFamily, SocketAddrUnix, SocketType};

use hex_api::{BoxId, Exit, Proc, Program, Refusal, Signal};
use hex_core::boxuser::{self, Bound, Use};
use hex_core::darwin;
use hex_core::helper::{self, Answer, Refused, Request, START_FDS, State};
use hex_core::pool::{Account, ROLE_UIDS, next_account};

use super::helper_client::{recv_framed, send_framed};
use super::sys;

mod kids;
use kids::{Full, Kids, Taken};

/// One live member of a box's user: its pid and its physical footprint in bytes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Member {
    pub pid: i32,
    pub footprint: u64,
}

/// The box user's live members, each with its footprint, through `proc_listpids(PROC_UID_ONLY)` and
/// `proc_pid_rusage`. Finds every process of the user — including one that daemonized (`setsid`) out of
/// its process group, since a user id is inherited and can't be dropped unprivileged
/// (docs/research/runtime/native-platforms.md §3). A process gone since it was listed (`ESRCH`) or ended
/// and awaiting reap is left out; its footprint would be 0.
pub fn members(uid: u32) -> io::Result<Vec<Member>> {
    let mut pids = sys::listing()?;
    sys::user_pids(uid, &mut pids)?;
    let mut out = Vec::with_capacity(pids.len());
    for &pid in &pids {
        if let Some(r) = sys::rusage(pid)?
            && !r.ended
        {
            out.push(Member {
                pid,
                footprint: r.footprint,
            });
        }
    }
    Ok(out)
}

/// Sends `signal` (a Linux signal number, mapped to macOS's as the rest of Hex maps them) to every
/// process of the box's user. Returns how many it signalled. Delete sends `KILL` then waits for the user
/// to empty ([`members`] goes empty) before freeing it; [`hex_core::helper::Request::Signal`] sends the
/// caller's signal.
pub fn kill_user(uid: u32, linux_signal: i32) -> io::Result<usize> {
    let macos = darwin::signal_to_macos(linux_signal)
        .ok_or_else(|| io::Error::other(format!("signal {linux_signal} has no macOS number")))?;
    let sig = rustix::process::Signal::from_named_raw(macos)
        .ok_or_else(|| io::Error::other(format!("{macos} isn't a signal this kernel names")))?;
    let mut pids = sys::listing()?;
    sys::user_pids(uid, &mut pids)?;
    let mut signalled = 0usize;
    for &pid in &pids {
        if let Some(p) = rustix::process::Pid::from_raw(pid) {
            match rustix::process::kill_process(p, sig) {
                Ok(()) => signalled = signalled.saturating_add(1),
                // Gone between the listing and the kill: not an error.
                Err(rustix::io::Errno::SRCH) => {}
                Err(e) => return Err(e.into()),
            }
        }
    }
    Ok(signalled)
}

/// What the daemon must do to a box's kernel limits after a read of its members: arm some at new ledger
/// limits, or end the box because its tree reached the bound.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Plan {
    /// (Re)arm each `(pid, limit)`: a member new since the last read, or one whose apportioned share
    /// changed. A member whose share is unchanged keeps its current kernel limit and isn't re-armed.
    Arm(Vec<(i32, u64)>),
    /// The members' footprints reached what may be apportioned: end the box (its footprint and bound).
    End { footprint: u64, bound: u64 },
}

/// One box the helper manages: its pool user, its bound and reserve, and the ledger limit each member is
/// armed at now. The daemon apportions the bound across the box's members ([`hex_core::boxuser`]) and
/// arms the kernel limits ([`super::sys`], privileged); this tracks what is armed so a read only re-arms
/// what changed, and keeps the box's peak footprint and how often a member crossed its share.
#[derive(Debug, Clone)]
pub struct Tracked {
    uid: u32,
    bound: Bound,
    /// Each live member's pid and the ledger limit it is armed at.
    armed: BTreeMap<i32, u64>,
    peak: u64,
    crossings: u64,
    ended_over: bool,
    /// Its processes' cap (`RLIMIT_NPROC` of its user), set when each program starts.
    nproc: u32,
}

impl Tracked {
    /// A box on pool user `uid`, bounded at `bound` bytes with `reserve` bytes held back for the fork
    /// arming window (`hex_core::boxuser::Bound`), its processes capped at `nproc`.
    pub fn new(uid: u32, bound: u64, reserve: u64, nproc: u32) -> Self {
        Self {
            uid,
            bound: Bound {
                limit: bound,
                reserve,
            },
            armed: BTreeMap::new(),
            peak: 0,
            crossings: 0,
            ended_over: false,
            nproc,
        }
    }

    pub fn uid(&self) -> u32 {
        self.uid
    }

    /// Apportions the bound across `members` (their footprints now) and returns what to arm: the members
    /// new or whose share changed. Forgets members gone since the last read. Ends the box if the tree
    /// reached the apportionable bound (HEX-MAC-14).
    pub fn reapportion(&mut self, members: &[Member]) -> Plan {
        let footprint = members
            .iter()
            .fold(0u64, |s, m| s.saturating_add(m.footprint));
        self.peak = self.peak.max(footprint);
        let uses: Vec<Use> = members
            .iter()
            .filter_map(|m| {
                u32::try_from(m.pid).ok().map(|pid| Use {
                    pid,
                    footprint: m.footprint,
                })
            })
            .collect();
        match boxuser::apportion(self.bound, &uses) {
            boxuser::Plan::End { footprint, bound } => {
                self.ended_over = true;
                Plan::End { footprint, bound }
            }
            boxuser::Plan::Arm(shares) => {
                // Forget members gone since the last read, so `armed` tracks only the live ones.
                let live: BTreeSet<i32> = members.iter().map(|m| m.pid).collect();
                self.armed.retain(|pid, _| live.contains(pid));
                let mut to_arm = Vec::new();
                for s in shares {
                    let pid = i32::try_from(s.pid).unwrap_or(-1);
                    if self.armed.get(&pid) != Some(&s.limit) {
                        self.armed.insert(pid, s.limit);
                        to_arm.push((pid, s.limit));
                    }
                }
                Plan::Arm(to_arm)
            }
        }
    }

    /// The ledger cap, in whole MiB, to arm a just-started member at before the box is re-apportioned: the
    /// whole apportionable bound (a single member may use it all until others claim their shares). Rounded
    /// down, so a sum of such caps never passes the bound.
    pub fn provisional_cap_mib(&self) -> u32 {
        u32::try_from(self.bound.apportionable() >> 20).unwrap_or(u32::MAX)
    }

    /// What the box holds now, for an inspect answer.
    pub fn state(&self, members: &[Member]) -> State {
        let footprint = members
            .iter()
            .fold(0u64, |s, m| s.saturating_add(m.footprint));
        State {
            members: u32::try_from(members.len()).unwrap_or(u32::MAX),
            footprint,
            peak: self.peak.max(footprint),
            processes: u32::try_from(members.len()).unwrap_or(u32::MAX),
            crossings: self.crossings,
            ended_over: self.ended_over,
        }
    }
}

/// The privileged operations the daemon needs root for, behind a seam (CLAUDE.md §9) so the dispatch runs
/// against a simulator in tests and the real syscalls ([`super::helper_sys`], `unsafe`, CLAUDE.md §12)
/// swap in. The membership reads and kills ([`members`], [`kill_user`]) need no privilege and are not here.
pub trait Privileged {
    /// Start `program` as user `uid` (a new session if `leader`), its stdio the three `fds`, its process
    /// count capped by `nproc` (`RLIMIT_NPROC` of the user) and its physical footprint armed at `cap_mib`
    /// with its `EXC_RESOURCE` routed to the daemon. Returns its pid.
    fn spawn(
        &self,
        uid: u32,
        program: &Program,
        fds: [OwnedFd; START_FDS],
        leader: bool,
        cap_mib: u32,
        nproc: u32,
    ) -> io::Result<i32>;

    /// Watch program `pid`, just started (the daemon is its parent), for its end and its reap.
    fn watch(&self, pid: i32) -> io::Result<Box<dyn Proc>>;

    /// Re-arm a running member's footprint at `cap_mib` (a forked member, or a rebalance of its share).
    fn arm(&self, pid: i32, cap_mib: u32) -> io::Result<()>;

    /// The box user's live members (needs no privilege; defaults to [`members`]).
    fn list(&self, uid: u32) -> io::Result<Vec<Member>> {
        members(uid)
    }

    /// Signal every process of the box user (needs no privilege when the node owns the pool; defaults to
    /// [`kill_user`]). Returns how many were signalled.
    fn kill(&self, uid: u32, signal: i32) -> io::Result<usize> {
        kill_user(uid, signal)
    }

    /// Watch each of the live members `pids` of user `uid`'s box for its forks and its end (`EVFILT_PROC`,
    /// which root may place on any process), each watch tagged with `uid`; a pid already watched is
    /// watched once. Returns those already gone.
    fn watch_members(&self, pids: &[i32], uid: u32) -> io::Result<Vec<i32>>;

    /// Blocks until a watched member forks or ends; returns the users (boxes) whose members did, by their
    /// watches' tags.
    fn next_member_event(&self) -> io::Result<Vec<u32>>;

    /// Blocks until a member's `EXC_RESOURCE` crossing arrives on the daemon's port.
    fn next_crossing(&self) -> io::Result<()>;

    /// The pool's users now, each name and uid, as the directory lists them: the users named by
    /// [`hex_core::pool::pool_index`] in the pool's group (default: none, a fixed pool).
    fn pool_users(&self) -> io::Result<Vec<(String, u32)>> {
        Ok(Vec::new())
    }

    /// Every role id held now (`ROLE_UIDS`, the only ids `next_account` weighs): users' uids, groups'
    /// gids and running processes' uids (default: none).
    fn taken_ids(&self) -> io::Result<BTreeSet<u32>> {
        Ok(BTreeSet::new())
    }

    /// Makes `account` from the helper's fixed template (hidden, no shell, no home, no password, in the pool's
    /// group) and records it for uninstall (default: refused, a backend that makes no accounts).
    fn make_user(&self, account: &Account) -> io::Result<()> {
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            format!("this backend makes no box users ({})", account.name),
        ))
    }

    /// The peak of concurrent box-user boxes kept across restarts (default: 0).
    fn peak(&self) -> io::Result<u32> {
        Ok(0)
    }

    /// Keeps a new peak of concurrent box-user boxes across restarts (default: kept nowhere).
    fn keep_peak(&self, _peak: u32) -> io::Result<()> {
        Ok(())
    }
}

/// The macOS box helper daemon over a [`Privileged`] backend: a pool of box users, the boxes it manages,
/// and the node's connection. A box's process count is `RLIMIT_NPROC` of its user (`nproc`), set when its
/// leader starts.
pub struct Daemon<P: Privileged> {
    ops: P,
    /// The free pool users the installer made (native-platforms.md §3); a box takes one on acquire and
    /// returns it on delete once its user is empty.
    pool: Mutex<Vec<u32>>,
    boxes: Mutex<BTreeMap<BoxId, Tracked>>,
    /// The programs the daemon started, until the node reads each one's end ([`kids`]).
    kids: Mutex<Kids>,
    /// The pool users' names, every one it holds, busy or free: the next account follows the highest
    /// (`hex_core::pool`). `None` for a fixed pool, which never grows.
    names: Option<Mutex<Vec<String>>>,
    /// The most box-user boxes held at once, kept across restarts through [`Privileged::keep_peak`].
    peak: Mutex<u32>,
    /// The platform's per-uid process ceiling (`kern.maxprocperuid`, read at start): a box's process cap
    /// when its spec names no `pids_max`. Each box has a uid of its own, and XNU's fork counts a uid's
    /// processes against its `RLIMIT_NPROC` (`bsd/kern/kern_fork.c:269-283`).
    per_uid: u32,
    /// How long a killed user gets to empty, as this node learns it: the record's to start, raised by every
    /// slower drain (hex_core::drain::Learned; the learned-waits rule, 2026-11-06).
    drain: Mutex<hex_core::drain::Learned>,
}

/// How many users a daemon's pool holds now, busy or free.
fn listed_len<P: Privileged>(daemon: &Daemon<P>) -> usize {
    daemon
        .names
        .as_ref()
        .and_then(|n| n.lock().ok().map(|n| n.len()))
        .unwrap_or(0)
}

impl<P: Privileged> Daemon<P> {
    /// A daemon over `ops`, with the free `pool` users under the platform's per-uid process ceiling
    /// `per_uid`. A box holds at most its process cap of programs the daemon started (its user can run no
    /// more), and the unread ends of as many ended boxes as the pool has users.
    pub fn new(ops: P, pool: Vec<u32>, per_uid: u32) -> Self {
        let graves = pool.len().max(1);
        Self {
            ops,
            pool: Mutex::new(pool),
            boxes: Mutex::new(BTreeMap::new()),
            kids: Mutex::new(Kids::new(graves)),
            names: None,
            peak: Mutex::new(0),
            per_uid,
            drain: Mutex::new(hex_core::drain::Learned::default()),
        }
    }

    /// A daemon whose pool grows on demand (the lead's call, 2026-11-06): its users are read from the
    /// directory, and accounts are made up to the kept peak of concurrent boxes, so a restart doesn't make
    /// boxes wait on account creation. Ended boxes' unread ends are kept for as many boxes as the role range
    /// holds users.
    pub fn growing(ops: P, per_uid: u32) -> Result<Self, Refusal> {
        let listed = ops
            .pool_users()
            .map_err(|e| Refusal::os("listing the box users' pool", e))?;
        let peak = ops
            .peak()
            .map_err(|e| Refusal::os("reading the box users' kept peak", e))?;
        let graves = ROLE_UIDS.clone().count();
        let daemon = Self {
            ops,
            pool: Mutex::new(listed.iter().map(|(_, uid)| *uid).collect()),
            boxes: Mutex::new(BTreeMap::new()),
            kids: Mutex::new(Kids::new(graves)),
            names: Some(Mutex::new(listed.into_iter().map(|(n, _)| n).collect())),
            peak: Mutex::new(peak),
            per_uid,
            drain: Mutex::new(hex_core::drain::Learned::default()),
        };
        // Counted to the kept peak; a full role range stops it short (the next box is refused by name).
        for _ in listed_len(&daemon)..usize::try_from(peak).unwrap_or(0) {
            let uid = match daemon.grow() {
                Ok(uid) => uid,
                Err(Refused::NoFreeUser) => break,
                Err(r) => {
                    return Err(Refusal::Broken(format!(
                        "growing the pool to its kept peak: {r}"
                    )));
                }
            };
            daemon
                .pool
                .lock()
                .map_err(|_| Refusal::Broken("the daemon's pool lock is poisoned".to_owned()))?
                .push(uid);
        }
        Ok(daemon)
    }

    /// Makes one more pool user, from `hex_core::pool` alone (no name or attribute from any caller), at an id
    /// read free just before: another installer may have taken it since install. Refused by name when the
    /// role range is full or the directory refuses.
    fn grow(&self) -> Result<u32, Refused> {
        let Some(names) = &self.names else {
            return Err(Refused::NoFreeUser);
        };
        let mut names = names.lock().map_err(|_| {
            Refused::GrowFailed("the daemon's pool names lock is poisoned".to_owned())
        })?;
        let taken = self
            .ops
            .taken_ids()
            .map_err(|e| Refused::GrowFailed(format!("reading the ids held: {e}")))?;
        let account = next_account(&names, &taken).ok_or(Refused::NoFreeUser)?;
        self.ops
            .make_user(&account)
            .map_err(|e| Refused::GrowFailed(format!("making {}: {e}", account.name)))?;
        names.push(account.name);
        Ok(account.uid)
    }

    /// Keeps a new peak when `held` box-user boxes are held at once.
    fn note_held(&self, held: usize) {
        let Ok(held) = u32::try_from(held) else {
            return;
        };
        if let Ok(mut peak) = self.peak.lock()
            && held > *peak
            && self.ops.keep_peak(held).is_ok()
        {
            *peak = held;
        }
    }

    /// Binds a `SOCK_STREAM` socket at `path` and serves node connections one at a time, forever (a node
    /// keeps a single connection; a later one waits, as the Linux node agent's socket serves its few local
    /// callers). A stale socket file is removed first. The boxes a connection made persist across its
    /// disconnect — the node ends them with [`Request::End`], and a reconnecting node finds them. Launchd
    /// may instead hand the daemon its socket; this path is for the lab and a plain install.
    pub fn listen(&self, path: &Path) -> Result<(), Refusal> {
        let listener = bind(path)?;
        self.serve_listener(listener.as_fd())
    }

    /// Serves node connections on an already-bound `listener`, one at a time, forever ([`Daemon::listen`]
    /// binds it; launchd or a test may bind it first). Each connection is served to its close.
    pub fn serve_listener(&self, listener: BorrowedFd<'_>) -> Result<(), Refusal> {
        loop {
            let conn = match rustix::net::accept(listener) {
                Ok(conn) => conn,
                Err(rustix::io::Errno::INTR) => continue,
                Err(e) => return Err(Refusal::os("accepting a helper connection", e)),
            };
            // macOS has no MSG_NOSIGNAL; SO_NOSIGPIPE keeps a dropped node from raising SIGPIPE on a send.
            let _ = rustix::net::sockopt::set_socket_nosigpipe(&conn, true);
            let _ = self.serve(conn.as_fd());
        }
    }

    /// One crossing turn: blocks until a member's `EXC_RESOURCE` crossing (the kernel ledger, the primary
    /// bound), then re-reads every box ([`Daemon::sweep`]). A crossing isn't mapped to one box — a non-fatal
    /// crossing arrives via a corpse whose pid isn't the member's — so every box is re-read, cheap at a
    /// node's box count. The daemon runs this on a thread of its own, beside its socket.
    pub fn pump_crossings(&self) -> Result<(), Refusal> {
        self.ops
            .next_crossing()
            .map_err(|e| Refusal::os("receiving a box crossing", e))?;
        self.sweep()
    }

    /// One membership turn: blocks until a watched member forks or ends, then re-reads its box, so a
    /// forked member is armed at its fork's notice and an ended one's share goes to the rest. This is the
    /// layer beneath the ledger, a box the ledger didn't hold — a member unarmed in the fork window — caught
    /// on the fork's own event, with no period to pick (the lead's call, 2026-11-06, under Ada's rule that
    /// no constant is picked). The daemon runs this on a thread of its own.
    pub fn pump_members(&self) -> Result<(), Refusal> {
        let touched = self
            .ops
            .next_member_event()
            .map_err(|e| Refusal::os("receiving a member's fork or end", e))?;
        self.sweep_users(Some(&touched))
    }

    /// Re-apportions every box (`rearm`: a box grows a member's share where headroom is free, and ends where
    /// the footprints reach the bound), watching each member it finds, and reaps the programs that ended.
    /// A per-box failure (a box ended since) is skipped, not fatal.
    fn sweep(&self) -> Result<(), Refusal> {
        self.sweep_users(None)
    }

    /// [`Daemon::sweep`] over the boxes of `users` only, or every box with `None`: a member's fork or end
    /// names its box by its watch's tag, so it costs that box's listing, not every box's.
    fn sweep_users(&self, users: Option<&[u32]>) -> Result<(), Refusal> {
        let boxes: Vec<(BoxId, u32)> = {
            let map = self
                .boxes
                .lock()
                .map_err(|_| Refusal::Broken("the daemon's boxes lock is poisoned".to_owned()))?;
            map.iter()
                .map(|(id, t)| (id.clone(), t.uid()))
                .filter(|(_, uid)| users.is_none_or(|u| u.contains(uid)))
                .collect()
        };
        for (id, uid) in boxes {
            let _ = self.rearm(&id, uid);
        }
        // Reap the programs that ended, so none stays a zombie under its user.
        self.kids()?.sweep();
        Ok(())
    }

    /// The box user's live members, each watched for its forks and its end. A member may fork after the
    /// listing and before its watch is placed, leaving a child whose forks post nothing; so it lists again
    /// after each watch until a listing finds no member it hadn't watched. Each further round watches a
    /// member new since the last, so it ends within the platform's per-uid process ceiling of rounds
    /// (`per_uid`, which no box's `RLIMIT_NPROC` passes); past that, the newest children's forks wake the next turn through their parents'
    /// watches.
    fn watched_members(&self, uid: u32) -> Result<Vec<Member>, Answer> {
        let list = |uid| {
            self.ops
                .list(uid)
                .map_err(|e| os("reading a box's members", &e))
        };
        let mut live = list(uid)?;
        for _ in 0..=self.per_uid {
            let pids: Vec<i32> = live.iter().map(|m| m.pid).collect();
            self.ops
                .watch_members(&pids, uid)
                .map_err(|e| os("watching a box's members", &e))?;
            let seen: BTreeSet<i32> = pids.into_iter().collect();
            let again = list(uid)?;
            let fresh = again.iter().any(|m| !seen.contains(&m.pid));
            live = again;
            if !fresh {
                break;
            }
        }
        Ok(live)
    }

    /// Serves one node connection until it closes: reads each framed request and answers it. A request it
    /// can't read is refused by a typed answer (CLAUDE.md §3); the connection closing (a read of nothing)
    /// ends the loop. The caller ends the boxes this connection held (the node's disconnect rule).
    pub fn serve(&self, fd: BorrowedFd<'_>) -> Result<(), Refusal> {
        loop {
            let (bytes, fds) = match recv_framed(fd) {
                Ok(m) => m,
                // The node closed (or the link broke): stop serving it.
                Err(_) => return Ok(()),
            };
            let (version, id, answer) = match helper::read_request(&bytes, fds.len()) {
                Ok((version, id, request)) => (version, id, self.handle(request, fds)),
                // Malformed: refuse it by name, in the first version (its own may be unreadable).
                Err(fault) => (
                    helper::FIRST,
                    0,
                    Answer::Refused(Refused::Malformed(fault.to_string())),
                ),
            };
            let out = helper::encode_answer(version, id, &answer)
                .map_err(|e| Refusal::Broken(format!("encoding a helper answer: {e}")))?;
            send_framed(fd, &out, &[])?;
        }
    }

    fn handle(&self, request: Request, fds: Vec<OwnedFd>) -> Answer {
        match request {
            Request::Hello { .. } => Answer::Hello {
                version: helper::VERSION,
            },
            Request::Acquire {
                box_id,
                bound,
                reserve,
                pids_max,
            } => self.acquire(box_id, bound, reserve, pids_max),
            Request::Start {
                box_id,
                program,
                leader,
            } => self.start(&box_id, &program, fds, leader),
            Request::End { box_id } => self.end(&box_id),
            Request::Inspect { box_id } => self.inspect(&box_id),
            Request::Signal { box_id, signal } => self.signal(&box_id, signal),
            Request::Wait { box_id, pid } => self.wait(&box_id, pid),
            Request::Kill { box_id, pid } => self.kill(&box_id, pid),
        }
    }

    fn boxes(&self) -> Result<std::sync::MutexGuard<'_, BTreeMap<BoxId, Tracked>>, Answer> {
        self.boxes
            .lock()
            .map_err(|_| poisoned("the daemon's boxes"))
    }

    fn kids(&self) -> Result<std::sync::MutexGuard<'_, Kids>, Refusal> {
        self.kids
            .lock()
            .map_err(|_| Refusal::Broken("the daemon's programs lock is poisoned".to_owned()))
    }

    fn acquire(&self, box_id: BoxId, bound: u64, reserve: u64, pids_max: Option<u32>) -> Answer {
        let mut boxes = match self.boxes() {
            Ok(b) => b,
            Err(a) => return a,
        };
        if boxes.contains_key(&box_id) {
            return Answer::Refused(Refused::AlreadyAcquired(box_id));
        }
        let Ok(mut pool) = self.pool.lock() else {
            return poisoned("the daemon's pool");
        };
        let popped = pool.pop();
        drop(pool);
        // No free user: the pool grows by one account, made for this box.
        let uid = match popped {
            Some(uid) => uid,
            None => match self.grow() {
                Ok(uid) => uid,
                Err(r) => return Answer::Refused(r),
            },
        };
        // The box's own limit, never past the platform's per-uid ceiling, which the kernel would hold it to.
        let nproc = pids_max.map_or(self.per_uid, |p| p.min(self.per_uid));
        match self.kids() {
            Ok(mut k) => k.open(&box_id, usize::try_from(nproc).unwrap_or(usize::MAX)),
            Err(_) => return poisoned("the daemon's programs"),
        }
        boxes.insert(box_id, Tracked::new(uid, bound, reserve, nproc));
        self.note_held(boxes.len());
        Answer::Acquired { uid }
    }

    fn start(&self, box_id: &BoxId, program: &Program, fds: Vec<OwnedFd>, leader: bool) -> Answer {
        let Ok([stdin, stdout, stderr]) = <[OwnedFd; START_FDS]>::try_from(fds) else {
            return Answer::Refused(Refused::Malformed(
                "a start without its three stdio descriptors".to_owned(),
            ));
        };
        let (uid, cap_mib, nproc) = {
            let boxes = match self.boxes() {
                Ok(b) => b,
                Err(a) => return a,
            };
            match boxes.get(box_id) {
                Some(t) => (t.uid(), t.provisional_cap_mib(), t.nproc),
                None => return Answer::Refused(Refused::UnknownBox(box_id.clone())),
            }
        };
        match self.kids().map(|mut k| k.room(box_id)) {
            Ok(Ok(())) => {}
            Ok(Err(Full::NoBox)) => return Answer::Refused(Refused::UnknownBox(box_id.clone())),
            Ok(Err(Full::Running(cap))) => {
                return Answer::Refused(Refused::StartFailed(format!(
                    "the box runs {cap} programs the helper started, its most"
                )));
            }
            Err(_) => return poisoned("the daemon's programs"),
        }
        // Spawn generously at the whole apportionable bound (the only member may use it all), then
        // re-apportion against the live membership and arm each member down to its share.
        let pid = match self.ops.spawn(
            uid,
            program,
            [stdin, stdout, stderr],
            leader,
            cap_mib,
            nproc,
        ) {
            Ok(pid) => pid,
            Err(e) => return Answer::Refused(Refused::StartFailed(e.to_string())),
        };
        // Watched from its start, so its end is seen and reaped however soon it comes.
        let held = self
            .ops
            .watch(pid)
            .map_err(|e| e.to_string())
            .and_then(|p| match self.kids() {
                Ok(mut k) => k.hold(box_id, pid, p).map_err(|f| format!("{f:?}")),
                Err(e) => Err(e.to_string()),
            });
        if let Err(why) = held {
            if let Some(p) = rustix::process::Pid::from_raw(pid) {
                let _ = rustix::process::kill_process(p, rustix::process::Signal::KILL);
            }
            return Answer::Refused(Refused::StartFailed(format!(
                "watching the started program {pid}: {why}; it was killed"
            )));
        }
        if let Err(a) = self.rearm(box_id, uid) {
            return a;
        }
        Answer::Started { pid }
    }

    /// Re-apportions a box against its live members and arms each one whose share changed; ends the box if
    /// its tree reached the bound. Called after a member starts, and on a crossing or a member's fork or end.
    fn rearm(&self, box_id: &BoxId, uid: u32) -> Result<(), Answer> {
        let live = self.watched_members(uid)?;
        let plan = {
            let mut boxes = self.boxes()?;
            let Some(tracked) = boxes.get_mut(box_id) else {
                return Ok(());
            };
            tracked.reapportion(&live)
        };
        match plan {
            Plan::Arm(to_arm) => {
                for (pid, limit) in to_arm {
                    let mib = u32::try_from(limit >> 20).unwrap_or(u32::MAX);
                    if let Err(e) = self.ops.arm(pid, mib) {
                        return Err(os("arming a box member", &e));
                    }
                }
                Ok(())
            }
            // The tree reached the bound: end the box. The caller's answer still reports the start that
            // tipped it; the next read finds the box gone.
            Plan::End { .. } => {
                let _ = self.end(box_id);
                Ok(())
            }
        }
    }

    fn end(&self, box_id: &BoxId) -> Answer {
        let uid = {
            let mut boxes = match self.boxes() {
                Ok(b) => b,
                Err(a) => return a,
            };
            match boxes.remove(box_id) {
                Some(t) => t.uid(),
                None => return Answer::Refused(Refused::UnknownBox(box_id.clone())),
            }
        };
        // Kill every process of the user, then wait for the user to empty before freeing it, so the next
        // box never inherits a live process. KILL is 9; mapped through the table like the rest of Hex. The
        // user's size sets how long it gets: the measured kill-to-empty envelope, doubled, as this node has
        // learned it (hex_core::drain); past it the kill didn't take, a fault named (CLAUDE.md §4). The user
        // can't be freed while it holds a process, so the node gets the refusal.
        let members = match self.ops.list(uid) {
            Ok(m) => m.len(),
            Err(e) => return os("listing a box's user", &e),
        };
        let patience = match self.drain.lock() {
            Ok(d) => d.bound(members),
            Err(_) => {
                return os(
                    "reading the node's drain bound",
                    &io::Error::other("poisoned"),
                );
            }
        };
        let killed = Instant::now();
        if let Err(e) = self.ops.kill(uid, Signal::KILL.number()) {
            return os("ending a box", &e);
        }
        let deadline = killed.checked_add(patience);
        loop {
            match self.ops.list(uid).map(|m| m.is_empty()) {
                Ok(true) => {
                    if let Ok(mut d) = self.drain.lock() {
                        d.observe(members, killed.elapsed());
                    }
                    break;
                }
                Ok(false) => {
                    if deadline.is_none_or(|d| Instant::now() >= d) {
                        return os(
                            "emptying a box's user",
                            &io::Error::other(format!("user {uid} still holds a live process")),
                        );
                    }
                    let _ = self.ops.kill(uid, Signal::KILL.number());
                    std::hint::spin_loop();
                }
                Err(e) => return os("checking a box's user is empty", &e),
            }
        }
        // Reap the box's programs (each a zombie now, or about to be) and keep their ends for the node.
        if let Ok(mut k) = self.kids() {
            k.close(box_id, Instant::now().checked_add(patience));
        }
        if let Ok(mut pool) = self.pool.lock() {
            pool.push(uid);
        }
        Answer::Ended
    }

    fn inspect(&self, box_id: &BoxId) -> Answer {
        let uid = {
            let boxes = match self.boxes() {
                Ok(b) => b,
                Err(a) => return a,
            };
            match boxes.get(box_id) {
                Some(t) => t.uid(),
                None => return Answer::Refused(Refused::UnknownBox(box_id.clone())),
            }
        };
        let live = match self.ops.list(uid) {
            Ok(m) => m,
            Err(e) => return os("reading a box's members", &e),
        };
        let mut boxes = match self.boxes() {
            Ok(b) => b,
            Err(a) => return a,
        };
        match boxes.get_mut(box_id) {
            Some(tracked) => Answer::State(tracked.state(&live)),
            None => Answer::Refused(Refused::UnknownBox(box_id.clone())),
        }
    }

    fn signal(&self, box_id: &BoxId, signal: i32) -> Answer {
        let uid = {
            let boxes = match self.boxes() {
                Ok(b) => b,
                Err(a) => return a,
            };
            match boxes.get(box_id) {
                Some(t) => t.uid(),
                None => return Answer::Refused(Refused::UnknownBox(box_id.clone())),
            }
        };
        // Refused as not running when the user has no live member (an ended one awaiting reap is not
        // one), as every class refuses it (HEX-DIFF-2).
        match self.ops.list(uid) {
            Ok(live) if live.is_empty() => {
                return Answer::Refused(Refused::NotRunning(box_id.clone()));
            }
            Ok(_) => {}
            Err(e) => return os("reading a box's members", &e),
        }
        match self.ops.kill(uid, signal) {
            Ok(0) => Answer::Refused(Refused::NotRunning(box_id.clone())),
            Ok(_) => Answer::Signalled,
            Err(e) => os("signalling a box", &e),
        }
    }
}

impl<P: Privileged> Daemon<P> {
    /// A [`Request::Kill`]: ends program `pid`, which the daemon started in box `box_id` and holds running,
    /// through its own watch of it (`Proc::kill`, `SIGKILL`; as root the daemon may signal the box's user),
    /// leaving it held so its end is read by a [`Request::Wait`] as any end is; the box and its other
    /// programs run on (HEX-RT-16). A pid the box doesn't hold running (ended, its end read, or never
    /// started there) is sent nothing, so a node can only kill its own box's programs: killing an ended
    /// program is done. An unknown box is refused by name.
    fn kill(&self, box_id: &BoxId, pid: i32) -> Answer {
        let killed = match self.kids() {
            Ok(mut k) => k.kill(box_id, pid),
            Err(_) => return poisoned("the daemon's programs"),
        };
        match killed {
            None => Answer::Refused(Refused::UnknownBox(box_id.clone())),
            Some(Ok(())) => Answer::Killed,
            Some(Err(e)) => Answer::Refused(Refused::StartFailed(format!("killing {pid}: {e}"))),
        }
    }

    /// A [`Request::Wait`]: the program's end, reaped here (the daemon is its parent), read once.
    fn wait(&self, box_id: &BoxId, pid: i32) -> Answer {
        let taken = match self.kids() {
            Ok(mut k) => k.take(box_id, pid),
            Err(_) => return poisoned("the daemon's programs"),
        };
        let mut proc = match taken {
            None => return Answer::Refused(Refused::UnknownBox(box_id.clone())),
            Some(None) => return Answer::Waited(Some(Exit::Unknown)),
            Some(Some(Taken::Ended(exit))) => return Answer::Waited(Some(exit)),
            Some(Some(Taken::Live(p))) => p,
        };
        // At once: the node asks after the program's exit event, which the daemon's own watch holds too,
        // so the reap finds the zombie its exit path makes; a running program answers `None`.
        let got = proc.try_wait();
        if !matches!(got, Ok(Some(_))) {
            let back = match self.kids() {
                Ok(mut k) => k.put_back(box_id, pid, proc),
                Err(_) => Err(proc),
            };
            // Its box was dropped meanwhile: reap it here rather than leave a zombie.
            if let Err(mut p) = back {
                let patience = self
                    .drain
                    .lock()
                    .map_or_else(|_| hex_core::drain::bound(1), |d| d.bound(1));
                let _ = p.wait_for(patience);
            }
        }
        match got {
            Ok(exit) => Answer::Waited(exit),
            Err(e) => Answer::Refused(Refused::StartFailed(format!("waiting on {pid}: {e}"))),
        }
    }
}

/// Binds a `SOCK_STREAM` helper socket at `path` (removing a stale one) and listens; the daemon serves on
/// it with [`Daemon::serve_listener`]. Split from [`Daemon::listen`] so a caller can bind before it serves
/// — launchd, or a test that connects a node to it once it is bound.
pub fn bind(path: &Path) -> Result<OwnedFd, Refusal> {
    let _ = rustix::fs::unlink(path);
    let fd = rustix::net::socket(AddressFamily::UNIX, SocketType::STREAM, None)
        .map_err(|e| Refusal::os("making the helper socket", e))?;
    let addr = SocketAddrUnix::new(path).map_err(|e| Refusal::os("naming the helper socket", e))?;
    rustix::net::bind(&fd, &addr).map_err(|e| Refusal::os("binding the helper socket", e))?;
    // The platform's own backlog bound (`SOMAXCONN`, `bsd/sys/socket.h:597`): a node keeps one connection, and
    // the kernel caps any larger request at `kern.ipc.somaxconn`.
    rustix::net::listen(&fd, libc::SOMAXCONN)
        .map_err(|e| Refusal::os("listening on the helper socket", e))?;
    Ok(fd)
}

fn poisoned(what: &str) -> Answer {
    Answer::Refused(Refused::Malformed(format!("{what} lock is poisoned")))
}

fn os(op: &str, e: &io::Error) -> Answer {
    Answer::Refused(Refused::StartFailed(format!("{op}: {e}")))
}

#[cfg(test)]
mod tests;
