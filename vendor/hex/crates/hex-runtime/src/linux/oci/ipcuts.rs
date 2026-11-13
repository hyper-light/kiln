//! The ipc and uts namespaces a node agent makes ahead for its containers (goal 5 extended, step 4). Making an
//! ipc namespace mounts a mqueue filesystem of its own and registers two sysctl tables of its own (Linux v6.19
//! `ipc/namespace.c:38-90`, `ipc/mqueue.c:469-489`, `ipc/ipc_sysctl.c:293`, `ipc/mq_sysctl.c:146`), the most a
//! create's clone pays for any one namespace (`bench/competitive/probes/levers.md`). A pair is made as decision 4
//! makes a network namespace ([`super::net::make_ahead`]): unshared on one thread of the agent, off the path, and
//! held by its descriptors. A container's init joins it by `setns` in place of its clone making them.

use std::os::fd::{AsFd, OwnedFd};

use rustix::fs::{Mode, OFlags};
use rustix::thread::UnshareFlags;

use crate::linux::oci::{OciError, sys};

/// An ipc namespace and a uts namespace, each held by its descriptor, close-on-exec ("As long as this file
/// descriptor remains open, the namespace will remain alive", namespaces(7)).
#[derive(Debug)]
pub struct Pair {
    pub ipc: OwnedFd,
    pub uts: OwnedFd,
}

impl Pair {
    /// The calling thread's own pair: a thread's namespaces are its own, and this is where a thread that makes
    /// pairs ahead goes back to after each.
    pub fn of_this_thread() -> Result<Self, OciError> {
        Ok(Self {
            ipc: open_ns(
                "/proc/thread-self/ns/ipc",
                "opening this thread's ipc namespace",
            )?,
            uts: open_ns(
                "/proc/thread-self/ns/uts",
                "opening this thread's uts namespace",
            )?,
        })
    }

    /// Moves the calling thread into the pair. `setns` checks each descriptor against the type it is named with
    /// (v6.19 `kernel/nsproxy.c:573-576`), so a pair handed over crossed is refused, never joined. It takes
    /// `CAP_SYS_ADMIN` in the namespace's owning user namespace (`ipc/namespace.c:233-244`,
    /// `kernel/utsname.c:127-140`), which is why a container in a user namespace of its own can't join a pair the
    /// node made and its clone makes its own.
    pub fn join(&self) -> Result<(), OciError> {
        sys::setns(self.ipc.as_fd(), libc::CLONE_NEWIPC)?;
        sys::setns(self.uts.as_fd(), libc::CLONE_NEWUTS).map_err(OciError::from)
    }
}

fn open_ns(path: &str, what: &'static str) -> Result<OwnedFd, OciError> {
    rustix::fs::open(path, OFlags::RDONLY | OFlags::CLOEXEC, Mode::empty())
        .map_err(|e| OciError::os(what, e))
}

/// A fresh pair for a container to join, made on the calling thread. The thread goes back to `home`, its own
/// pair, before this returns, whether or not the make worked, so it holds no namespace it makes and each ends with
/// the container it went to; and the next pair's names are copied from `home`'s, never from a uts namespace a
/// container has set a hostname in (`kernel/utsname.c:58`). A thread that can't go back is refused as the make is;
/// its next make moves it on.
pub fn make_ahead(home: &Pair) -> Result<Pair, OciError> {
    sys::unshare(UnshareFlags::NEWIPC | UnshareFlags::NEWUTS)?;
    let made = Pair::of_this_thread();
    let back = home.join();
    match (made, back) {
        (Ok(pair), Ok(())) => Ok(pair),
        (Err(e), _) | (Ok(_), Err(e)) => Err(e),
    }
}
