//! The existing length-prefixed HEXM stream, with bounded storage and readiness-driven I/O.
use std::io::{self, IoSlice, IoSliceMut};
use std::mem::MaybeUninit;
use std::os::fd::{AsFd, BorrowedFd, OwnedFd};
use std::time::{Duration, Instant};

use hex_api::Refusal;
use hex_core::helper::{START_FDS, managed};
use rustix::event::{PollFd, PollFlags, Timespec};
use rustix::io::{Errno, FdFlags};
use rustix::net::{
    RecvAncillaryBuffer, RecvAncillaryMessage, RecvFlags, ReturnFlags, SendAncillaryBuffer,
    SendAncillaryMessage, SendFlags,
};

const LENGTH: usize = size_of::<u32>();
// XNU f6217f891ac0bb64f3d375211650a4c1ff8ca1ea, uipc_usrreq.c:
// UIPC_MAX_CMSG_FD bounds one local rights record. soreceive stops data at the next control
// boundary. Receive the full native range before refusing >protocol3; copyout_control truncation
// does not supply ownership of unseen descriptor numbers. This is not a new protocol allowance.
const NATIVE_RIGHTS: usize = 512;

#[derive(Debug, thiserror::Error)]
pub(crate) enum Failure {
    #[error("native helper framed I/O failed: {0}")]
    Io(io::Error),
    #[error("helper frame could not reserve {bytes} bytes")]
    Allocation { bytes: usize },
    #[error("helper frame has {bytes} bytes past its {bound}-byte bound")]
    Length { bytes: usize, bound: usize },
    #[error("helper frame carries {count} descriptors")]
    Descriptors { count: usize },
    #[error("helper ancillary data was truncated")]
    TruncatedControl,
    #[error("helper frame counter is unrepresentable")]
    Counter,
    #[error("helper closed before completing its frame")]
    Closed,
}

impl Failure {
    pub(crate) fn managed(&self, operation: managed::Operation) -> managed::Failure {
        match self {
            Self::Io(error) => managed::Failure::Io {
                operation,
                code: error.raw_os_error(),
                kind: error.kind(),
            },
            Self::Allocation { bytes } => match u64::try_from(*bytes) {
                Ok(bytes) => managed::Failure::Allocation { bytes },
                Err(_) => managed::Failure::Control,
            },
            Self::Closed => managed::Failure::Uncertain,
            Self::Length { .. }
            | Self::Descriptors { .. }
            | Self::TruncatedControl
            | Self::Counter => managed::Failure::Control,
        }
    }

    pub(crate) fn refusal(self) -> Refusal {
        match self {
            Self::Io(error) => Refusal::os("the helper's framed I/O", error),
            Self::Allocation { bytes } => Refusal::Exhausted {
                what: "helper frame storage",
                bound: bytes,
            },
            Self::Length { bound, .. } => Refusal::Exhausted {
                what: "helper frame bytes",
                bound,
            },
            other => {
                let text = match other {
                    Self::Descriptors { .. } => "the helper frame carries too many descriptors",
                    Self::TruncatedControl => "the helper frame has truncated native control",
                    Self::Counter => "the helper frame has an unrepresentable length or count",
                    Self::Closed => "the helper closed before a whole frame",
                    Self::Io(_) | Self::Allocation { .. } | Self::Length { .. } => {
                        "the helper frame is invalid"
                    }
                };
                let mut why = String::new();
                if why.try_reserve_exact(text.len()).is_err() {
                    return Refusal::Exhausted {
                        what: "helper frame refusal bytes",
                        bound: text.len(),
                    };
                }
                why.push_str(text);
                Refusal::Broken(why)
            }
        }
    }
}

fn deadline(until: Option<Instant>) -> Result<(), Failure> {
    if until.is_some_and(|until| Instant::now() >= until) {
        return Err(Failure::Io(io::ErrorKind::TimedOut.into()));
    }
    Ok(())
}

/// None is the explicit event-only wait, never a guessed protocol timeout.
pub(crate) fn wait(
    fd: BorrowedFd<'_>,
    events: PollFlags,
    until: Option<Instant>,
) -> Result<(), Failure> {
    loop {
        deadline(until)?;
        let timeout = match until {
            None => None,
            Some(until) => {
                // Darwin poll takes a signed millisecond timeout. Long FINITE caller deadlines
                // use native-width chunks and re-evaluate that same deadline, never INFINITE.
                let maximum =
                    Duration::from_millis(u64::try_from(i32::MAX).map_err(|_| Failure::Counter)?);
                let left = until.saturating_duration_since(Instant::now()).min(maximum);
                Some(Timespec::try_from(left).map_err(|_| Failure::Counter)?)
            }
        };
        let mut watched = [PollFd::from_borrowed_fd(fd, events)];
        match rustix::event::poll(&mut watched, timeout.as_ref()) {
            Ok(0) => deadline(until)?,
            Ok(_) => {
                let flags = watched.first().ok_or(Failure::Counter)?.revents();
                if flags.intersects(PollFlags::NVAL) {
                    return Err(Failure::Io(io::ErrorKind::InvalidInput.into()));
                }
                if flags.intersects(events | PollFlags::HUP | PollFlags::ERR) {
                    return Ok(());
                }
            }
            Err(Errno::INTR) => {}
            Err(error) => return Err(Failure::Io(error.into())),
        }
    }
}

pub(crate) fn send(
    fd: BorrowedFd<'_>,
    message: &[u8],
    descriptors: &[BorrowedFd<'_>],
    bound: usize,
    until: Option<Instant>,
) -> Result<(), Failure> {
    if message.len() > bound {
        return Err(Failure::Length {
            bytes: message.len(),
            bound,
        });
    }
    if descriptors.len() > START_FDS {
        return Err(Failure::Descriptors {
            count: descriptors.len(),
        });
    }
    let length = u32::try_from(message.len()).map_err(|_| Failure::Counter)?;
    let room = LENGTH.checked_add(message.len()).ok_or(Failure::Counter)?;
    let mut frame = Vec::new();
    frame
        .try_reserve_exact(room)
        .map_err(|_| Failure::Allocation { bytes: room })?;
    frame.extend_from_slice(&length.to_le_bytes());
    frame.extend_from_slice(message);
    let mut sent = 0usize;
    while sent < frame.len() {
        deadline(until)?;
        let mut space = [MaybeUninit::uninit(); rustix::cmsg_space!(ScmRights(START_FDS))];
        let mut control = SendAncillaryBuffer::new(&mut space);
        if sent == 0
            && !descriptors.is_empty()
            && !control.push(SendAncillaryMessage::ScmRights(descriptors))
        {
            return Err(Failure::Descriptors {
                count: descriptors.len(),
            });
        }
        let bytes = frame.get(sent..).ok_or(Failure::Counter)?;
        match rustix::net::sendmsg(
            fd,
            &[IoSlice::new(bytes)],
            &mut control,
            SendFlags::DONTWAIT,
        ) {
            Ok(0) => return Err(Failure::Closed),
            Ok(count) if count <= bytes.len() => {
                sent = sent.checked_add(count).ok_or(Failure::Counter)?;
            }
            Ok(_) => return Err(Failure::Counter),
            Err(Errno::INTR) => {}
            Err(Errno::AGAIN) => wait(fd, PollFlags::OUT, until)?,
            Err(error) => return Err(Failure::Io(error.into())),
        }
    }
    Ok(())
}

pub(crate) fn receive(
    fd: BorrowedFd<'_>,
    bound: usize,
    until: Option<Instant>,
) -> Result<(Vec<u8>, Vec<OwnedFd>), Failure> {
    let mut descriptors = Vec::new();
    descriptors
        .try_reserve_exact(START_FDS)
        .map_err(|_| Failure::Allocation {
            bytes: START_FDS.saturating_mul(size_of::<OwnedFd>()),
        })?;
    let mut prefix = [0u8; LENGTH];
    exact(fd, &mut prefix, &mut descriptors, until)?;
    let length = usize::try_from(u32::from_le_bytes(prefix)).map_err(|_| Failure::Counter)?;
    if length > bound {
        return Err(Failure::Length {
            bytes: length,
            bound,
        });
    }
    let mut message = Vec::new();
    message
        .try_reserve_exact(length)
        .map_err(|_| Failure::Allocation { bytes: length })?;
    message.resize(length, 0);
    exact(fd, &mut message, &mut descriptors, until)?;
    Ok((message, descriptors))
}

fn exact(
    fd: BorrowedFd<'_>,
    output: &mut [u8],
    descriptors: &mut Vec<OwnedFd>,
    until: Option<Instant>,
) -> Result<(), Failure> {
    let mut used = 0usize;
    while used < output.len() {
        deadline(until)?;
        let mut space = [MaybeUninit::uninit(); rustix::cmsg_space!(ScmRights(NATIVE_RIGHTS))];
        let mut control = RecvAncillaryBuffer::new(&mut space);
        let bytes = output.get_mut(used..).ok_or(Failure::Counter)?;
        let received = match rustix::net::recvmsg(
            fd,
            &mut [IoSliceMut::new(bytes)],
            &mut control,
            RecvFlags::DONTWAIT,
        ) {
            Ok(received) => received,
            Err(Errno::INTR) => continue,
            Err(Errno::AGAIN) => {
                wait(fd, PollFlags::IN, until)?;
                continue;
            }
            Err(error) => return Err(Failure::Io(error.into())),
        };
        let mut total = descriptors.len();
        let mut failure = None;
        for message in control.drain() {
            if let RecvAncillaryMessage::ScmRights(rights) = message {
                for owned in rights {
                    total = total.checked_add(1).ok_or(Failure::Counter)?;
                    if total > START_FDS || failure.is_some() {
                        drop(owned);
                        continue;
                    }
                    // SCM_RIGHTS may change the number. This sets only the receiver's owned
                    // descriptor flag, never its sender's CLOEXEC or the shared pipe's status.
                    if let Err(error) = rustix::io::fcntl_setfd(owned.as_fd(), FdFlags::CLOEXEC) {
                        failure = Some(Failure::Io(error.into()));
                        drop(owned);
                    } else {
                        descriptors.push(owned);
                    }
                }
            }
        }
        if received.flags.contains(ReturnFlags::CTRUNC) {
            return Err(Failure::TruncatedControl);
        }
        if total > START_FDS {
            return Err(Failure::Descriptors { count: total });
        }
        if let Some(failure) = failure {
            return Err(failure);
        }
        if received.bytes == 0 {
            return Err(Failure::Closed);
        }
        if received.bytes > bytes.len() {
            return Err(Failure::Counter);
        }
        used = used.checked_add(received.bytes).ok_or(Failure::Counter)?;
    }
    Ok(())
}

#[cfg(test)]
mod controls {
    use super::*;
    use rustix::net::{AddressFamily, SocketFlags, SocketType};
    use std::os::fd::AsRawFd;

    fn private_pipe() -> (OwnedFd, OwnedFd) {
        let (reader, writer) = std::io::pipe().unwrap();
        let (reader, writer) = (OwnedFd::from(reader), OwnedFd::from(writer));
        for end in [&reader, &writer] {
            crate::macos::private_descriptor(end, false).unwrap();
        }
        (reader, writer)
    }

    fn pair() -> (OwnedFd, OwnedFd) {
        let pair = rustix::net::socketpair(
            AddressFamily::UNIX,
            SocketType::STREAM,
            SocketFlags::empty(),
            None,
        )
        .unwrap();
        for fd in [&pair.0, &pair.1] {
            crate::macos::private_descriptor(fd, true).unwrap();
            rustix::net::sockopt::set_socket_nosigpipe(fd, true).unwrap();
        }
        pair
    }
    fn guard() -> Option<Instant> {
        Some(Instant::now() + Duration::from_secs(1))
    }

    /// AC-T7.4.1: legacy length/body bytes and exactly allowlisted rights survive the same framing;
    /// the final receiver-owned descriptor differs, is CLOEXEC, and never changes parent flags.
    #[test]
    fn framed_rights_keep_legacy_bytes_and_receiver_owned_descriptor_flags() {
        let (writer, reader) = pair();
        let (pipe_read, pipe_write) = private_pipe();
        let flags = rustix::io::fcntl_getfd(&pipe_write).unwrap();
        send(
            writer.as_fd(),
            b"HEXMlegacy",
            &[pipe_write.as_fd()],
            64,
            guard(),
        )
        .unwrap();
        let (body, rights) = receive(reader.as_fd(), 64, guard()).unwrap();
        assert_eq!(body, b"HEXMlegacy");
        assert_eq!(rights.len(), 1);
        assert_ne!(rights[0].as_raw_fd(), pipe_write.as_raw_fd());
        assert!(
            rustix::io::fcntl_getfd(&rights[0])
                .unwrap()
                .contains(FdFlags::CLOEXEC)
        );
        assert_eq!(rustix::io::fcntl_getfd(&pipe_write).unwrap(), flags);
        rustix::io::write(&rights[0], b"x").unwrap();
        let mut byte = [0];
        assert_eq!(rustix::io::read(&pipe_read, &mut byte).unwrap(), 1);
        assert_eq!(&byte, b"x");
        send(writer.as_fd(), b"next", &[], 64, guard()).unwrap();
        assert_eq!(receive(reader.as_fd(), 64, guard()).unwrap().0, b"next");
    }
    /// AC-T7.4.1/R01: an over-bound prefix is refused before its body, partial EOF never becomes a
    /// whole message, and excessive native rights are closed/refused rather than silently ignored.
    #[test]
    fn framed_partial_oversized_and_extra_rights_never_become_messages() {
        let (writer, reader) = pair();
        rustix::io::write(&writer, &65u32.to_le_bytes()).unwrap();
        assert!(matches!(
            receive(reader.as_fd(), 64, guard()),
            Err(Failure::Length {
                bytes: 65,
                bound: 64
            })
        ));
        let (writer, reader) = pair();
        rustix::io::write(&writer, &3u32.to_le_bytes()).unwrap();
        rustix::io::write(&writer, b"a").unwrap();
        drop(writer);
        assert!(matches!(
            receive(reader.as_fd(), 64, guard()),
            Err(Failure::Closed)
        ));
        let (writer, reader) = pair();
        let (pipe_read, pipe_write) = private_pipe();
        let rights = [
            pipe_read.as_fd(),
            pipe_write.as_fd(),
            pipe_read.as_fd(),
            pipe_write.as_fd(),
        ];
        let mut space = [MaybeUninit::uninit(); rustix::cmsg_space!(ScmRights(4))];
        let mut control = SendAncillaryBuffer::new(&mut space);
        assert!(control.push(SendAncillaryMessage::ScmRights(&rights)));
        let mut bytes = 1u32.to_le_bytes().to_vec();
        bytes.push(b'x');
        rustix::net::sendmsg(
            &writer,
            &[IoSlice::new(&bytes)],
            &mut control,
            SendFlags::DONTWAIT,
        )
        .unwrap();
        assert!(matches!(
            receive(reader.as_fd(), 64, guard()),
            Err(Failure::Descriptors { count: 4 })
        ));
        assert!(send(writer.as_fd(), b"x", &rights, 64, guard()).is_err());
    }
}
