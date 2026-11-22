//! The process's descriptor budget (docs/design/local-owner-transport.md, "One descriptor budget"):
//! the file descriptors this process may still open, from the system's own limit as it stands, that
//! what Amphora holds open charges as it opens it. Connections accepted by the local owners charge
//! one each (their socket), so no count of connections is picked: the system's limit is the bound.
//!
//! Nothing is read once and kept: the limit is read again at every charge (a host may raise it while
//! it runs), and the descriptors open outside the budget (the host's own, the owners' files) are
//! counted again whenever the limit moves, a charge would be refused, or the system refuses a
//! descriptor the budget allowed (`EMFILE`).

use amphora_api::Refusal;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, OnceLock};

/// The descriptors that may be held at once against what the system allows, and those held now.
pub(crate) struct Descriptors {
    /// The system's, read again as it runs; or a test's fixed limit, with nothing open outside.
    system: bool,
    /// The limit as last read.
    limit: AtomicUsize,
    /// The descriptors open that this budget doesn't hold, as last counted.
    outside: AtomicUsize,
    held: AtomicUsize,
}

/// Descriptors held against a budget, given back when this drops.
pub(crate) struct Held {
    budget: Arc<Descriptors>,
    count: usize,
}

impl Descriptors {
    /// The system's budget: its limit and what is open now.
    fn system() -> Result<Arc<Self>, String> {
        let budget = Arc::new(Self {
            system: true,
            limit: AtomicUsize::new(limit()),
            outside: AtomicUsize::new(0),
            held: AtomicUsize::new(0),
        });
        budget.recount()?;
        Ok(budget)
    }

    /// A budget of `limit` descriptors with none open outside it: a test's.
    #[cfg(test)]
    pub(crate) fn fixed(limit: usize) -> Arc<Self> {
        Arc::new(Self {
            system: false,
            limit: AtomicUsize::new(limit),
            outside: AtomicUsize::new(0),
            held: AtomicUsize::new(0),
        })
    }

    /// The most descriptors this budget may hold at once, as it last saw the system: its limit
    /// less what is open outside it.
    pub(crate) fn bound(&self) -> usize {
        self.limit
            .load(Ordering::Acquire)
            .saturating_sub(self.outside.load(Ordering::Acquire))
    }

    /// The descriptors held now.
    #[cfg(test)]
    pub(crate) fn held(&self) -> usize {
        self.held.load(Ordering::Acquire)
    }

    /// `count` more held, refused typed (`what`, past the bound) when they would pass it. The
    /// system's limit is read first, and what is open outside counted again if it moved; a charge
    /// that would be refused is tried once more after a fresh count, since the host may have let
    /// descriptors go.
    pub(crate) fn take(self: &Arc<Self>, what: &str, count: usize) -> Result<Held, Refusal> {
        if self.system {
            let now = limit();
            if self.limit.swap(now, Ordering::AcqRel) != now {
                self.recount().map_err(Refusal::Unavailable)?;
            }
        }
        let held = match self.try_take(count) {
            Ok(()) => Ok(()),
            Err(_) if self.system => {
                self.recount().map_err(Refusal::Unavailable)?;
                self.try_take(count)
            }
            Err(past) => Err(past),
        };
        held.map_err(|past| Refusal::TooLarge {
            what: what.into(),
            size: past,
            limit: self.bound(),
        })?;
        Ok(Held {
            budget: Arc::clone(self),
            count,
        })
    }

    /// The system refused a descriptor this budget allowed (`EMFILE`): more is open outside it
    /// than last counted, so it is counted again.
    pub(crate) fn refused_by_system(&self) -> Result<(), Refusal> {
        if self.system {
            self.recount().map_err(Refusal::Unavailable)?;
        }
        Ok(())
    }

    /// `count` more held if within the bound; else what the held would come to.
    fn try_take(&self, count: usize) -> Result<(), usize> {
        let mut past = 0;
        self.held
            .try_update(Ordering::AcqRel, Ordering::Acquire, |held| {
                let after = held.checked_add(count);
                past = after.unwrap_or(usize::MAX);
                after.filter(|after| *after <= self.bound())
            })
            .map(drop)
            .map_err(|_| past)
    }

    /// What is open outside the budget, counted now: what the process holds less what the budget
    /// does. A charge or give-back between the two readings makes it off by those, for one count:
    /// one too many held refuses a charge until the next count, one too few meets the system's
    /// own refusal (`refused_by_system`).
    fn recount(&self) -> Result<(), String> {
        let open = open()?;
        let outside = open.saturating_sub(self.held.load(Ordering::Acquire));
        self.outside.store(outside, Ordering::Release);
        Ok(())
    }
}

impl Drop for Held {
    fn drop(&mut self) {
        // Never below none: each give-back follows its own take.
        let _given = self
            .budget
            .held
            .try_update(Ordering::AcqRel, Ordering::Acquire, |held| {
                Some(held.saturating_sub(self.count))
            });
    }
}

/// The process's budget, the system's; a test's own, where it set one for what it opens on its
/// thread.
pub(crate) fn process() -> Result<Arc<Descriptors>, Refusal> {
    #[cfg(test)]
    if let Some(budget) = tests::BUDGET.with(|budget| budget.borrow().clone()) {
        return Ok(budget);
    }
    static PROCESS: OnceLock<Result<Arc<Descriptors>, String>> = OnceLock::new();
    PROCESS
        .get_or_init(Descriptors::system)
        .clone()
        .map_err(Refusal::Unavailable)
}

/// The soft `RLIMIT_NOFILE` (POSIX `getrlimit`): the most descriptors the system lets this process
/// hold. With none set (`RLIM_INFINITY`) nothing here bounds them, and the kernel's own ceiling
/// (Linux's `fs.nr_open`) refuses past it.
#[cfg(unix)]
fn limit() -> usize {
    rustix::process::getrlimit(rustix::process::Resource::Nofile)
        .current
        .map_or(usize::MAX, |soft| {
            usize::try_from(soft).unwrap_or(usize::MAX)
        })
}

/// Windows has no descriptor limit a process can set: its handle table holds at most 2^24
/// handles a process (Mark Russinovich, "Pushing the Limits of Windows: Handles", Microsoft,
/// 2009, the handle table's three levels), and Winsock's sockets are handles out of the same table
/// (`WSADATA::iMaxSockets` is ignored since Winsock 2, Microsoft's WSAStartup documentation).
#[cfg(windows)]
fn limit() -> usize {
    1 << 24
}

/// The descriptors this process holds now: the entries of `/dev/fd` (Linux's link to
/// `/proc/self/fd`, macOS's fdesc), the one this count opens among them, so it counts one more
/// than it leaves.
#[cfg(unix)]
fn open() -> Result<usize, String> {
    let entries = std::fs::read_dir("/dev/fd")
        .map_err(|error| format!("this process's open descriptors (/dev/fd): {error}"))?;
    Ok(entries.count())
}

/// Windows says a process's open handles only through `GetProcessHandleCount`, which this crate
/// can't call without `unsafe` (CLAUDE.md section 12): none counted, so a handle the process holds
/// already comes out of the same table, and the system refuses past it.
#[cfg(windows)]
fn open() -> Result<usize, String> {
    Ok(0)
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    std::thread_local! {
        /// A test's own budget, for what it opens on its thread.
        pub(crate) static BUDGET: std::cell::RefCell<Option<Arc<Descriptors>>> =
            const { std::cell::RefCell::new(None) };
    }

    /// Sets this thread's budget to one of `bound` descriptors, for what the test opens on it.
    pub(crate) fn budget(bound: usize) -> Arc<Descriptors> {
        let budget = Descriptors::fixed(bound);
        BUDGET.with(|set| *set.borrow_mut() = Some(Arc::clone(&budget)));
        budget
    }

    /// AC-FD.1.1, T-FD.1.1 (local-owner-transport.md, "One descriptor budget"): a budget holds
    /// what it was given and no more. Do: take a budget of 3 one, then two; ask one more; give the
    /// two back and ask two. Expect: the first three held; the fourth refused, naming what it was
    /// for, 4 past 3; once given back, two more held.
    #[test]
    fn a_budget_holds_its_bound_and_takes_back_what_is_given() {
        let budget = Descriptors::fixed(3);
        let one = budget.take("a connection", 1).unwrap();
        let two = budget.take("a connection", 2).unwrap();
        let refused = budget.take("a connection", 1).err().unwrap();
        assert_eq!(
            refused,
            Refusal::TooLarge {
                what: "a connection".into(),
                size: 4,
                limit: 3
            }
        );
        drop(two);
        let _again = budget.take("a connection", 2).unwrap();
        drop(one);
        assert!(budget.take("a connection", 2).is_err());
    }

    /// AC-FD.1.1, T-FD.1.2 (local-owner-transport.md, "One descriptor budget"): the process's
    /// budget is the system's limit as it stands less what is open outside it, never a number
    /// read once. Do: a system budget on Unix; raise the soft RLIMIT_NOFILE by one (where the hard
    /// limit allows) and charge nothing; put it back and charge nothing; ask for more than any
    /// limit. Expect: room under the limit by stdin, stdout and stderr at least; the budget's
    /// limit one more after the raise and back after; the ask refused typed.
    #[cfg(unix)]
    #[test]
    fn the_process_budget_follows_the_systems_limit() {
        use rustix::process::{Resource, getrlimit, setrlimit};
        let budget = Descriptors::system().unwrap();
        let soft = limit();
        let room = budget.bound();
        assert!(
            room > 0 && soft.saturating_sub(room) >= 3,
            "room {room} of {soft}"
        );
        let was = getrlimit(Resource::Nofile);
        if let (Some(current), Some(maximum)) = (was.current, was.maximum)
            && current < maximum
        {
            let mut raised = was;
            raised.current = Some(current + 1);
            setrlimit(Resource::Nofile, raised).unwrap();
            drop(budget.take("a connection", 0).unwrap());
            let followed = budget.limit.load(Ordering::Acquire);
            setrlimit(Resource::Nofile, was).unwrap();
            assert_eq!(followed, soft + 1, "the raised limit wasn't read");
            drop(budget.take("a connection", 0).unwrap());
            assert_eq!(budget.limit.load(Ordering::Acquire), soft);
        }
        assert!(matches!(
            budget.take("a connection", usize::MAX / 2),
            Err(Refusal::TooLarge { .. })
        ));
    }
}
