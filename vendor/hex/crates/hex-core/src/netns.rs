//! The node agent's pool of network namespaces made ahead (decision 4 of docs/design/runtime-architecture.md,
//! goal 4 plan step 3): how many it holds, when it asks for more, what a create's take is answered with, and
//! what it counts. Pure: the agent's refill thread makes each namespace off the path (`unshare(CLONE_NEWNET)` on
//! that thread, loopback brought up inside it) and keeps it alive by its descriptor, the shell's `T`; a create
//! whose container makes a network namespace of its own takes one, and its container joins it with `setns`. This
//! decides which, and refuses by type what can't be done.
//!
//! - **The bound**: the pool holds at most its size: learned from its draws, up to the bound its shell derives,
//!   unless the node is given a fixed one ([`Pool::learning`], [`Pool::new`]; Ada's rule, 2026-11-06). A
//!   namespace made past it is refused ([`Refused::Full`]) and handed back for the shell to close.
//! - **The refill**: the refill mark is one below the bound. The pool asks for more as soon as it holds fewer
//!   than its bound ([`Pool::wanted`]), as decision 4's one thread refills whenever the pool is below it. A make
//!   that fails stops the asking until the next take, so a kernel that refuses every make costs that thread one
//!   failed make a take, never a loop of them (CLAUDE.md §2).
//! - **Never reused**: a take moves its namespace out to the caller and nothing hands one back, so nothing of
//!   one container's network reaches the next and none needs cleaning. Takes go oldest first, so under churn
//!   every namespace is handed out within `bound` takes of its making, none left in the pool while others
//!   come and go.
//! - **Dry**: a take from an empty pool is answered [`Take::MakeOnPath`] and counted; the create makes its
//!   namespace on the path, as with no pool. A create the pool can't serve, a container with a user namespace
//!   of its own (decision 4), makes its namespace on the path too, and [`Pool::on_path`] counts it the same.
//! - **Failed makes**: each is reported with the step of the make that failed ([`Step`]) and why ([`Reason`]: the
//!   errno and the error's text), counted by step ([`FailedBy`]), and the latest reason kept for each step
//!   ([`LastFailed`], one a step, so what is kept stays bounded), so a make the kernel refuses is never only a
//!   number (CLAUDE.md §3).
//! - **Refused**: a node that lacks a capability a make takes ([`Missing`]) starts no pool whose every make
//!   would fail ([`Pool::refused`]): it holds none and asks for none, and every take is made on the path,
//!   counted under that reason ([`Counts::refused_on_path`]).
//! - **Shutdown**: [`Pool::shut_down`] hands back what the pool holds, counted as discarded, and every take after
//!   it is refused ([`ShutDown`]); a namespace made after it is handed back ([`Refused::ShutDown`]).
//!
//! [`Counts`] is what the bench and the node's metrics read.
//!
//! The node's ipc and uts namespaces made ahead use this pool too (goal 5 extended, step 4): `T` is then a pair, and
//! its size is learned from its draws ([`Pool::learning`]; Ada's rule, 2026-11-06).

use std::collections::VecDeque;

use crate::demand::Depth;

/// The step of a make ahead that failed: why it failed, as the shell reports it ([`Pool::failed`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Step {
    /// Opening the making thread's own namespace, which it goes back to after each make.
    Home,
    /// `unshare(CLONE_NEWNET)`, which takes CAP_SYS_ADMIN (unshare(2)).
    Unshare,
    /// Opening the namespace made, the descriptor that keeps it alive.
    Open,
    /// Bringing its loopback up, an rtnetlink request that takes CAP_NET_ADMIN (rtnetlink(7)).
    Loopback,
    /// Going back to the thread's own namespace, `setns`, which takes CAP_SYS_ADMIN (setns(2)).
    Back,
}

/// Failed makes, counted by the [`Step`] that failed; they sum to [`Counts::failed`].
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct FailedBy {
    pub home: u64,
    pub unshare: u64,
    pub open: u64,
    pub loopback: u64,
    pub back: u64,
}

impl FailedBy {
    fn count(&mut self, step: Step) {
        let n = match step {
            Step::Home => &mut self.home,
            Step::Unshare => &mut self.unshare,
            Step::Open => &mut self.open,
            Step::Loopback => &mut self.loopback,
            Step::Back => &mut self.back,
        };
        *n = n.saturating_add(1);
    }
}

impl std::fmt::Display for Step {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Step::Home => "opening its thread's own namespace",
            Step::Unshare => "unshare",
            Step::Open => "opening the namespace made",
            Step::Loopback => "bringing its loopback up",
            Step::Back => "setns back home",
        })
    }
}

/// The most bytes of a failed make's text a [`Reason`] keeps: a longer one is cut at a character boundary and
/// the cut marked `…`. A stated bound, not a measured one: the longest text a make gives today, a loopback
/// refusal with its OS error, is under 100 bytes, and 256 holds it with room to spare.
pub const MAX_REASON: usize = 256;

/// Why a make failed at its step: the errno the kernel gave, where there was one, and the error's text, at most
/// [`MAX_REASON`] bytes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Reason {
    errno: Option<i32>,
    text: String,
}

impl Reason {
    /// The reason for `errno` and `text`, the text cut to [`MAX_REASON`] bytes.
    pub fn new(errno: Option<i32>, text: &str) -> Self {
        let text = if text.len() <= MAX_REASON {
            text.to_owned()
        } else {
            let keep = MAX_REASON.saturating_sub('…'.len_utf8());
            let cut = text
                .char_indices()
                .map(|(at, c)| at.saturating_add(c.len_utf8()))
                .take_while(|end| *end <= keep)
                .last()
                .unwrap_or(0);
            format!("{}…", text.get(..cut).unwrap_or_default())
        };
        Self { errno, text }
    }

    /// The errno, where the kernel gave one.
    pub fn errno(&self) -> Option<i32> {
        self.errno
    }

    /// The error's text.
    pub fn text(&self) -> &str {
        &self.text
    }
}

impl std::fmt::Display for Reason {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self.errno {
            Some(errno) => write!(f, "errno {errno}: {}", self.text),
            None => f.write_str(&self.text),
        }
    }
}

/// The latest [`Reason`] a make failed for at each [`Step`]: one a step, replaced by the next.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct LastFailed {
    pub home: Option<Reason>,
    pub unshare: Option<Reason>,
    pub open: Option<Reason>,
    pub loopback: Option<Reason>,
    pub back: Option<Reason>,
}

impl LastFailed {
    fn keep(&mut self, step: Step, reason: Reason) {
        let slot = match step {
            Step::Home => &mut self.home,
            Step::Unshare => &mut self.unshare,
            Step::Open => &mut self.open,
            Step::Loopback => &mut self.loopback,
            Step::Back => &mut self.back,
        };
        *slot = Some(reason);
    }
}

/// What a node lacks of the capabilities a make ahead takes, in its own user namespace: CAP_SYS_ADMIN for
/// `unshare` and `setns`, CAP_NET_ADMIN for loopback's rtnetlink request. At least one is missing: a node
/// that holds both has none ([`Missing::of`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Missing {
    sys_admin: bool,
    net_admin: bool,
}

impl Missing {
    /// What a node that holds CAP_SYS_ADMIN when `sys_admin` and CAP_NET_ADMIN when `net_admin` lacks, or
    /// nothing when it holds both.
    pub fn of(sys_admin: bool, net_admin: bool) -> Option<Self> {
        if sys_admin && net_admin {
            None
        } else {
            Some(Self {
                sys_admin: !sys_admin,
                net_admin: !net_admin,
            })
        }
    }

    /// Whether CAP_SYS_ADMIN is missing.
    pub fn sys_admin(self) -> bool {
        self.sys_admin
    }

    /// Whether CAP_NET_ADMIN is missing.
    pub fn net_admin(self) -> bool {
        self.net_admin
    }
}

impl std::fmt::Display for Missing {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match (self.sys_admin, self.net_admin) {
            (true, true) => "CAP_SYS_ADMIN and CAP_NET_ADMIN",
            (true, false) => "CAP_SYS_ADMIN",
            _ => "CAP_NET_ADMIN",
        })
    }
}

/// What a take is answered with.
#[derive(Debug, PartialEq, Eq)]
pub enum Take<T> {
    /// A namespace made ahead, now the caller's alone: the pool never has it again.
    Pooled(T),
    /// The pool is dry: the create makes its container's namespace on the path. Counted in
    /// [`Counts::made_on_path`].
    MakeOnPath,
}

/// A take refused: the pool is shut down, so the node is going away and starts no container.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[error("the network-namespace pool is shut down")]
pub struct ShutDown;

/// A namespace the pool didn't take in, handed back for the shell to close, and why.
#[derive(Debug, PartialEq, Eq, thiserror::Error)]
pub enum Refused<T> {
    /// The pool holds its bound already. A shell that makes only what [`Pool::wanted`] asks for, one at a time,
    /// never makes one past it.
    #[error("the network-namespace pool holds its most ({bound})")]
    Full { bound: usize, ns: T },
    /// The pool is shut down: the make was under way when it was.
    #[error("the network-namespace pool is shut down")]
    ShutDown { ns: T },
}

/// What the pool has done, as the bench and the node's metrics read it. The pool counts only namespaces it took
/// in, so every one made ahead was taken, is held or was discarded: `made_ahead` is `taken`, plus
/// [`Pool::held`], plus `discarded`. A count stops at `u64::MAX`, which at one a nanosecond takes 584 years to
/// reach.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Counts {
    /// Namespaces made ahead and taken into the pool.
    pub made_ahead: u64,
    /// Namespaces handed to creates.
    pub taken: u64,
    /// Namespaces made on the path: takes the pool was dry for, and creates it can't serve
    /// ([`Pool::on_path`]).
    pub made_on_path: u64,
    /// Namespaces the pool held when it was shut down, handed back to close, never used.
    pub discarded: u64,
    /// Makes that failed.
    pub failed: u64,
    /// Makes that failed, by the step that failed.
    pub failed_by: FailedBy,
    /// The latest reason a make failed for, at each step.
    pub last_failed: LastFailed,
    /// What the node lacks, when its pool was refused for it ([`Pool::refused`]): it makes none ahead.
    pub refused: Option<Missing>,
    /// Of `made_on_path`, the takes made on the path because the pool was refused.
    pub refused_on_path: u64,
}

/// The pool: the namespaces it holds as the shell's `T`, whether it asks for more, and what it has done. The
/// shell keeps it behind its lock; a take neither allocates nor waits.
#[derive(Debug)]
pub struct Pool<T> {
    /// Its size: fixed ([`Pool::new`]) or learned from its draws ([`Pool::learning`]).
    bound: Depth,
    /// Fresh namespaces, oldest first, never more than `bound`.
    held: VecDeque<T>,
    /// False from a failed make to the next take: the bound on the refill's retries.
    asking: bool,
    shut: bool,
    counts: Counts,
}

impl<T> Pool<T> {
    /// An empty pool that holds at most `bound` namespaces and asks for all of them. With a bound of 0 it holds
    /// none, and every take is made on the path, as on a node without a pool.
    pub fn new(bound: usize) -> Self {
        Self::learning(bound, bound)
    }

    /// An empty pool of `start` that learns its size from its draws, up to `cap` (Ada's rule, 2026-11-06): a take
    /// that finds it empty raises it to the takes since it was last full plus one for each found short
    /// ([`Depth`]), so the next draw as deep finds it full. It never falls, so no draw already seen runs it short
    /// again.
    pub fn learning(start: usize, cap: usize) -> Self {
        Self {
            bound: Depth::new(start, cap),
            held: VecDeque::new(),
            asking: true,
            shut: false,
            counts: Counts::default(),
        }
    }

    /// How many namespaces the pool asks its shell to make now: the room below its bound, while it is open and
    /// no make has failed since the last take. The shell makes them off the path, one at a time, and hands each
    /// in with [`made`](Self::made) or reports it [`failed`](Self::failed). The pool counts no make under way,
    /// so a second maker could make one past the bound, which is refused.
    pub fn wanted(&self) -> usize {
        if self.shut || !self.asking {
            return 0;
        }
        // `held` never passes the bound, so this is the room left.
        self.bound.size().saturating_sub(self.held.len())
    }

    /// A namespace the shell made ahead: taken in and counted, or, past the bound or after shutdown, refused
    /// and handed back.
    pub fn made(&mut self, ns: T) -> Result<(), Refused<T>> {
        if self.shut {
            return Err(Refused::ShutDown { ns });
        }
        if self.held.len() >= self.bound.size() {
            return Err(Refused::Full {
                bound: self.bound.size(),
                ns,
            });
        }
        self.held.push_back(ns);
        if self.held.len() >= self.bound.size() {
            self.bound.full();
        }
        self.counts.made_ahead = self.counts.made_ahead.saturating_add(1);
        Ok(())
    }

    /// A pool refused because the node lacks what a make takes (`missing`): it holds none and asks for none,
    /// and every take is made on the path, counted under that reason.
    pub fn refused(missing: Missing) -> Self {
        let mut pool = Self::new(0);
        pool.counts.refused = Some(missing);
        pool
    }

    /// A make that failed at `step` for `reason`: counted by step, the reason kept as that step's latest, and the
    /// pool asks for no more until the next take.
    pub fn failed(&mut self, step: Step, reason: Reason) {
        self.counts.failed = self.counts.failed.saturating_add(1);
        self.counts.failed_by.count(step);
        self.counts.last_failed.keep(step, reason);
        self.asking = false;
    }

    /// A create's namespace: the oldest made ahead, now the caller's alone, or, when the pool is dry, word to
    /// make it on the path, counted. Either way the pool asks for more. Refused once the pool is shut down.
    pub fn take(&mut self) -> Result<Take<T>, ShutDown> {
        if self.shut {
            return Err(ShutDown);
        }
        self.asking = true;
        let taken = self.held.pop_front();
        self.bound.took(taken.is_none());
        Ok(match taken {
            Some(ns) => {
                self.counts.taken = self.counts.taken.saturating_add(1);
                Take::Pooled(ns)
            }
            None => {
                self.counts.made_on_path = self.counts.made_on_path.saturating_add(1);
                if self.counts.refused.is_some() {
                    self.counts.refused_on_path = self.counts.refused_on_path.saturating_add(1);
                }
                Take::MakeOnPath
            }
        })
    }

    /// A create the pool can't serve, which makes its namespace on the path: its container has a user namespace
    /// of its own, and a namespace belongs to its maker's (decision 4). Counted as a dry take is; nothing is
    /// taken, so the asking stays as it was. Refused once the pool is shut down, as a take is.
    pub fn on_path(&mut self) -> Result<(), ShutDown> {
        if self.shut {
            return Err(ShutDown);
        }
        self.counts.made_on_path = self.counts.made_on_path.saturating_add(1);
        Ok(())
    }

    /// Shuts the pool down and hands back what it holds for the shell to close, counted as discarded. Every
    /// take after is refused and nothing more is asked for; shutting down again hands back nothing. The shell
    /// shuts the pool down before it lets it go, so nothing it held goes uncounted.
    #[must_use = "the namespaces handed back are the shell's to close"]
    pub fn shut_down(&mut self) -> Vec<T> {
        self.shut = true;
        let held = Vec::from(std::mem::take(&mut self.held));
        let n = u64::try_from(held.len()).unwrap_or(u64::MAX);
        self.counts.discarded = self.counts.discarded.saturating_add(n);
        held
    }

    /// Its size now: the most it holds.
    pub fn bound(&self) -> usize {
        self.bound.size()
    }

    /// How many namespaces the pool holds.
    pub fn held(&self) -> usize {
        self.held.len()
    }

    /// Whether the pool is shut down: the refill thread's word to end.
    pub fn is_shut_down(&self) -> bool {
        self.shut
    }

    /// What the pool has done.
    pub fn counts(&self) -> Counts {
        self.counts.clone()
    }
}

#[cfg(test)]
mod tests;
