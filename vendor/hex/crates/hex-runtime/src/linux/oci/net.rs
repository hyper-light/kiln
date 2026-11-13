//! A container's own network namespace starts with its loopback down, so even `127.0.0.1` is out of
//! reach in it: measured on the running hardware (Linux 7.2), a fresh namespace's `lo` has flags 0x8,
//! LOOPBACK alone, and a connect to 127.0.0.1 fails with ENETUNREACH. runc and crun bring it up as
//! they make the namespace, and so does Hex, with one rtnetlink request (rtnetlink(7)): RTM_NEWLINK
//! setting IFF_UP on interface 1, which loopback is in every namespace (`LOOPBACK_IFINDEX`,
//! include/net/flow.h, which drivers/net/loopback.c checks as it registers it). The request is built,
//! and the kernel's answer read, as bytes, so this needs no `unsafe` (CLAUDE.md §12).
//!
//! A node agent makes such namespaces ahead of the containers that join them ([`make_ahead`]; decision 4
//! of docs/design/runtime-architecture.md), its loopback brought up the same way.

use std::os::fd::{BorrowedFd, OwnedFd};

use hex_core::netns::{Reason, Step};

use rustix::fs::{Mode, OFlags};
use rustix::net::{AddressFamily, RecvFlags, SendFlags, SocketFlags, SocketType};
use rustix::thread::UnshareFlags;

use crate::linux::oci::{OciError, sys};

/// `RTM_NEWLINK` (include/uapi/linux/rtnetlink.h).
const RTM_NEWLINK: u16 = 16;
/// `NLMSG_ERROR`, the type of the kernel's answer, an ack when its error is 0
/// (include/uapi/linux/netlink.h).
const NLMSG_ERROR: u16 = 2;
/// `NLM_F_REQUEST | NLM_F_ACK` (include/uapi/linux/netlink.h).
const REQUEST_WITH_ACK: u16 = 0x1 | 0x4;
/// `IFF_UP` (include/uapi/linux/if.h).
const IFF_UP: u32 = 0x1;
/// Loopback's interface index in every network namespace (`LOOPBACK_IFINDEX`).
const LOOPBACK_IFINDEX: i32 = 1;
/// The request: a `struct nlmsghdr` (16 bytes) and a `struct ifinfomsg` (16 bytes).
const REQUEST_BYTES: u32 = 32;
/// Room for the kernel's answer: a `struct nlmsghdr`, the error, and the request echoed back.
const ANSWER_BYTES: usize = 1024;

/// Brings up the loopback interface of the network namespace this process is in.
pub fn loopback_up() -> Result<(), OciError> {
    loopback().map_err(|failed| failed.error)
}

/// [`loopback_up`], refused with the errno where the kernel gave one.
fn loopback() -> Result<(), MakeFailed> {
    let failed = MakeFailed::at(Step::Loopback);
    let fd = rustix::net::socket_with(
        AddressFamily::NETLINK,
        SocketType::RAW,
        SocketFlags::CLOEXEC,
        None,
    )
    .map_err(|e| failed(Some(e), OciError::os("opening a route netlink socket", e)))?;
    let request = request();
    let sent = rustix::net::send(&fd, &request, SendFlags::empty())
        .map_err(|e| failed(Some(e), OciError::os("asking for loopback up", e)))?;
    if sent != request.len() {
        return Err(failed(
            None,
            OciError::os(
                "asking for loopback up",
                format!("sent {sent} of {} bytes", request.len()),
            ),
        ));
    }
    let mut answer = [0u8; ANSWER_BYTES];
    let (got, _) = rustix::net::recv(&fd, &mut answer, RecvFlags::empty()).map_err(|e| {
        failed(
            Some(e),
            OciError::os("reading the answer to loopback up", e),
        )
    })?;
    acked(answer.get(..got).unwrap_or_default())
}

/// The calling thread's network namespace, open close-on-exec (`/proc/thread-self`, proc(5)): a
/// thread's namespaces are its own, and this is where a thread that makes namespaces ahead goes back
/// to after each.
pub fn thread_network() -> Result<OwnedFd, OciError> {
    open_network(Step::Home).map_err(|failed| failed.error)
}

/// The making thread's own network namespace, which [`make_ahead`] goes back to: [`thread_network`],
/// refused as a make's [`Step::Home`].
pub fn home() -> Result<OwnedFd, MakeFailed> {
    open_network(Step::Home)
}

/// The calling thread's network namespace, refused as a make's `step`, with its errno.
fn open_network(step: Step) -> Result<OwnedFd, MakeFailed> {
    rustix::fs::open(
        "/proc/thread-self/ns/net",
        OFlags::RDONLY | OFlags::CLOEXEC,
        Mode::empty(),
    )
    .map_err(|e| {
        MakeFailed::at(step)(
            Some(e),
            OciError::os("opening this thread's network namespace", e),
        )
    })
}

/// A fresh network namespace for a container to join (decision 4), made on the calling thread with
/// `unshare(CLONE_NEWNET)` as containerd makes a pod's (`pkg/netns/netns_linux.go` at v2.2.0), its
/// loopback brought up inside it, and held by the descriptor returned, close-on-exec ("As long as
/// this file descriptor remains open, the namespace will remain alive", namespaces(7)). The thread
/// goes back to `home`, its own namespace ([`thread_network`]), before this returns, whether or not
/// the make worked, so it holds none of the namespaces it makes and each ends with the container it
/// went to. A thread that can't go back is refused as the make is; its next make moves it on. A make
/// refused is refused with the step that failed, the first one where two did.
pub fn make_ahead(home: BorrowedFd<'_>) -> Result<OwnedFd, MakeFailed> {
    sys::unshare(UnshareFlags::NEWNET)
        .map_err(|f| MakeFailed::at(Step::Unshare)(Some(f.errno), f.into()))?;
    let made = open_network(Step::Open).and_then(|ns| loopback().map(|()| ns));
    let back = sys::setns(home, libc::CLONE_NEWNET)
        .map_err(|f| MakeFailed::at(Step::Back)(Some(f.errno), f.into()));
    match (made, back) {
        (Ok(ns), Ok(())) => Ok(ns),
        (Err(e), _) | (Ok(_), Err(e)) => Err(e),
    }
}

/// A make ahead refused: the step that failed, the errno where the kernel gave one, and the error.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("making a network namespace ahead failed at {step}: {error}")]
pub struct MakeFailed {
    pub step: Step,
    pub errno: Option<i32>,
    pub error: OciError,
}

impl MakeFailed {
    /// A closure that names `step` as the one that failed, with the errno and error it is given.
    pub fn at(step: Step) -> impl Fn(Option<rustix::io::Errno>, OciError) -> Self {
        move |errno, error| Self {
            step,
            errno: errno.map(rustix::io::Errno::raw_os_error),
            error,
        }
    }

    /// Why it failed, as the pool keeps it: the errno and the error's text, cut to its bound.
    pub fn reason(&self) -> Reason {
        Reason::new(self.errno, &self.error.to_string())
    }
}

/// RTM_NEWLINK for interface 1, IFF_UP in its flags and in the mask of flags it changes, the kernel
/// asked to acknowledge it.
fn request() -> Vec<u8> {
    let mut m = Vec::with_capacity(32);
    // struct nlmsghdr: length, type, flags, sequence, and port 0, the kernel.
    m.extend_from_slice(&REQUEST_BYTES.to_ne_bytes());
    m.extend_from_slice(&RTM_NEWLINK.to_ne_bytes());
    m.extend_from_slice(&REQUEST_WITH_ACK.to_ne_bytes());
    m.extend_from_slice(&1u32.to_ne_bytes());
    m.extend_from_slice(&0u32.to_ne_bytes());
    // struct ifinfomsg: family AF_UNSPEC and padding, device type, index, flags, change mask.
    m.extend_from_slice(&[0, 0]);
    m.extend_from_slice(&0u16.to_ne_bytes());
    m.extend_from_slice(&LOOPBACK_IFINDEX.to_ne_bytes());
    m.extend_from_slice(&IFF_UP.to_ne_bytes());
    m.extend_from_slice(&IFF_UP.to_ne_bytes());
    m
}

/// Whether the kernel's answer acknowledges the request: an NLMSG_ERROR whose error is 0. A negative
/// error is the errno that refused it; anything else isn't an answer to it.
fn acked(answer: &[u8]) -> Result<(), MakeFailed> {
    let failed = MakeFailed::at(Step::Loopback);
    let unexpected = || {
        failed(
            None,
            OciError::os(
                "bringing loopback up",
                format!(
                    "the kernel's answer isn't an ack: {:?}",
                    answer.get(..32).unwrap_or(answer)
                ),
            ),
        )
    };
    let kind = answer
        .get(4..6)
        .and_then(|b| <[u8; 2]>::try_from(b).ok())
        .map(u16::from_ne_bytes)
        .ok_or_else(unexpected)?;
    let error = answer
        .get(16..20)
        .and_then(|b| <[u8; 4]>::try_from(b).ok())
        .map(i32::from_ne_bytes)
        .ok_or_else(unexpected)?;
    match (kind, error) {
        (NLMSG_ERROR, 0) => Ok(()),
        // The kernel's errno, negated; one outside 1..=MAX_ERRNO (4095, include/linux/err.h) is no
        // errno, and isn't an answer to the request.
        (NLMSG_ERROR, refused) if refused < 0 => match refused.checked_neg().filter(|n| *n <= 4095)
        {
            Some(errno) => Err(MakeFailed {
                step: Step::Loopback,
                errno: Some(errno),
                error: OciError::os(
                    "bringing loopback up",
                    std::io::Error::from_raw_os_error(errno),
                ),
            }),
            None => Err(unexpected()),
        },
        _ => Err(unexpected()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The constants are the kernel's, as the libc crate also has them.
    #[test]
    fn the_constants_are_the_kernels() {
        assert_eq!(RTM_NEWLINK, libc::RTM_NEWLINK);
        assert_eq!(i32::from(NLMSG_ERROR), libc::NLMSG_ERROR);
        assert_eq!(
            i32::from(REQUEST_WITH_ACK),
            libc::NLM_F_REQUEST | libc::NLM_F_ACK
        );
        assert_eq!(IFF_UP, u32::try_from(libc::IFF_UP).unwrap());
    }

    /// The request is RTM_NEWLINK for interface 1 with IFF_UP set and changed, asking for an ack.
    #[test]
    fn the_request_sets_interface_one_up() {
        let m = request();
        assert_eq!(m.len(), 32);
        let u32_at = |i: usize| u32::from_ne_bytes(m[i..i + 4].try_into().unwrap());
        let u16_at = |i: usize| u16::from_ne_bytes(m[i..i + 2].try_into().unwrap());
        assert_eq!(u32_at(0), 32, "nlmsg_len");
        assert_eq!(u16_at(4), RTM_NEWLINK, "nlmsg_type");
        assert_eq!(u16_at(6), 0x5, "NLM_F_REQUEST | NLM_F_ACK");
        assert_eq!(m[16], 0, "AF_UNSPEC");
        assert_eq!(
            i32::from_ne_bytes(m[20..24].try_into().unwrap()),
            1,
            "ifi_index"
        );
        assert_eq!(u32_at(24), IFF_UP, "ifi_flags");
        assert_eq!(u32_at(28), IFF_UP, "ifi_change");
    }

    fn answer(kind: u16, error: i32) -> Vec<u8> {
        let mut a = Vec::new();
        a.extend_from_slice(&36u32.to_ne_bytes());
        a.extend_from_slice(&kind.to_ne_bytes());
        a.extend_from_slice(&0u16.to_ne_bytes());
        a.extend_from_slice(&1u32.to_ne_bytes());
        a.extend_from_slice(&0u32.to_ne_bytes());
        a.extend_from_slice(&error.to_ne_bytes());
        a
    }

    /// An ack is the kernel's yes; a negative error is its no, as that errno; anything else, hostile
    /// bytes included, is a typed error, never a panic (CLAUDE.md §6).
    #[test]
    fn only_an_ack_is_a_yes() {
        assert!(acked(&answer(NLMSG_ERROR, 0)).is_ok());
        let refused = acked(&answer(NLMSG_ERROR, -libc::EPERM)).unwrap_err();
        assert!(refused.to_string().contains("not permitted"), "{refused}");
        assert_eq!(
            (refused.step, refused.errno),
            (Step::Loopback, Some(libc::EPERM))
        );
        // An errno past MAX_ERRNO is no errno: the answer is refused as not an ack, never trusted.
        let past = acked(&answer(NLMSG_ERROR, -4096)).unwrap_err();
        assert_eq!(past.errno, None, "{past}");
        for hostile in [
            Vec::new(),
            vec![0; 5],
            vec![0xff; 19],
            answer(RTM_NEWLINK, 0),
            answer(NLMSG_ERROR, 7),
            answer(NLMSG_ERROR, i32::MIN),
        ] {
            assert!(acked(&hostile).is_err(), "{hostile:?}");
        }
    }
}
