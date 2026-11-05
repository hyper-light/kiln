//! The platform stream-socket seam (docs/runtime.md §5.2), the shape of [`crate::netsys`]: paired `#[cfg]`
//! arms with one surface, so `crate::tcp` holds the async logic and only the system calls differ. A
//! [`Stream`] owns one non-blocking TCP socket of one family (IPv6 ones `IPV6_V6ONLY`); every call returns
//! [`Io`], so "not ready yet" is a value the caller awaits on.
//!
//! The Unix arm is `rustix`, with `libc` for the options it lacks (`TCP_NOTSENT_LOWAT`, macOS's
//! retransmission deadline, `TCP_CONGESTION`, Linux's `TCP_INFO` and `SIOCOUTQ`, macOS's
//! `TCP_CONNECTION_INFO`); the Windows arm is Winsock 2, sharing [`crate::netsys`]'s helpers. Reads and
//! writes go straight between the socket and the caller's slices, vectored ones included: `readv`/`writev`,
//! and `WSARecv`/`WSASend`, whose `WSABUF` array is the layout `IoSliceMut`/`IoSlice` guarantee on Windows.
#![allow(unsafe_code)]

use crate::error::RtError;
use crate::netsys::{Family, SocketAddr};

/// Which half of a stream to shut.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Shutdown {
    /// No more reads.
    Read,
    /// No more writes: the peer reads end of stream.
    Write,
    /// Both.
    Both,
}

/// Format: the longest congestion-control name Linux keeps, `TCP_CA_NAME_MAX` (16) less its NUL.
pub const CONGESTION_NAME_MAX: usize = 15;

/// Format: the buffer a congestion-control name is read into, `TCP_CA_NAME_MAX` (the name and its NUL).
const CONGESTION_NAME_BUF: usize = 16;

/// A congestion-control algorithm's name as the kernel spells it ("cubic", "reno", "bbr"): 1 to 15
/// printable ASCII bytes, kept inline (bp-g3 step 1, plan `01a1a832eb2f711f9c6f51d88effe89d`).
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub struct CongestionName {
    bytes: [u8; CONGESTION_NAME_BUF],
    len: u8,
}

impl CongestionName {
    /// The name, or `None` when it is empty, longer than [`CONGESTION_NAME_MAX`] bytes, or holds anything
    /// but printable ASCII (the kernel would cut a longer one short and pick another algorithm).
    pub fn new(name: &str) -> Option<Self> {
        let raw = name.as_bytes();
        if raw.is_empty() || raw.len() > CONGESTION_NAME_MAX || !raw.iter().all(u8::is_ascii_graphic) {
            return None;
        }
        let mut bytes = [0u8; CONGESTION_NAME_BUF];
        bytes.get_mut(..raw.len())?.copy_from_slice(raw);
        Some(Self {
            bytes,
            len: u8::try_from(raw.len()).ok()?,
        })
    }

    /// The name read back from the kernel: the bytes before the first NUL.
    #[cfg_attr(not(target_os = "linux"), allow(dead_code))]
    fn from_kernel(buf: &[u8]) -> Option<Self> {
        let end = buf.iter().position(|b| *b == 0).unwrap_or(buf.len());
        Self::new(core::str::from_utf8(buf.get(..end)?).ok()?)
    }

    /// The name.
    pub fn as_str(&self) -> &str {
        self.bytes
            .get(..usize::from(self.len))
            .and_then(|b| core::str::from_utf8(b).ok())
            .unwrap_or("")
    }

    /// The bytes the kernel is handed, without a NUL.
    #[cfg_attr(not(target_os = "linux"), allow(dead_code))]
    fn as_bytes(&self) -> &[u8] {
        self.bytes.get(..usize::from(self.len)).unwrap_or(&[])
    }
}

impl core::fmt::Debug for CongestionName {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(f, "{:?}", self.as_str())
    }
}

impl core::fmt::Display for CongestionName {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Why the kernel refused a congestion control.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CongestionRefusal {
    /// The algorithm is loaded, but an unprivileged process may not pick it: it isn't in
    /// `net.ipv4.tcp_allowed_congestion_control` (`EPERM`, tcp(7)). The host's administrator adds it.
    NotAllowed,
    /// No such algorithm is loaded (`ENOENT`): its module (`tcp_bbr`, say) isn't loaded, or there is none
    /// of that name. The host's administrator loads it.
    NotLoaded,
    /// Another refusal, with its OS error code.
    Os(Option<i32>),
}

/// A congestion control the kernel refused, naming the algorithm and the reason; the socket keeps the one
/// it had (bp-g3 step 1).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CongestionError {
    /// The algorithm asked for.
    pub algorithm: CongestionName,
    /// Why it was refused.
    pub reason: CongestionRefusal,
}

impl core::fmt::Display for CongestionError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self.reason {
            CongestionRefusal::NotAllowed => write!(
                f,
                "congestion control {} is not allowed (not in net.ipv4.tcp_allowed_congestion_control)",
                self.algorithm
            ),
            CongestionRefusal::NotLoaded => write!(
                f,
                "congestion control {} is not loaded (no such module or algorithm in the kernel)",
                self.algorithm
            ),
            CongestionRefusal::Os(code) => write!(
                f,
                "congestion control {} was refused by the OS (code {code:?})",
                self.algorithm
            ),
        }
    }
}

impl std::error::Error for CongestionError {}

/// What the kernel says of one TCP connection now (bp-g3 step 1): each field `None` where the platform
/// doesn't give it. Linux's `TCP_INFO` gives every field (the newer ones from the kernel that added them);
/// macOS's `TCP_CONNECTION_INFO` the smoothed RTT, window, segment size and retransmitted bytes; Windows'
/// `SIO_TCP_INFO` (version 0) RTTs, in-flight bytes, window, segment size and retransmitted bytes.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct TcpInfo {
    /// Bytes written that the kernel hasn't sent yet (Linux `tcpi_notsent_bytes`, 4.6).
    pub unsent_bytes: Option<u32>,
    /// Bytes sent and not yet acknowledged (Linux: `SIOCOUTQ` less the unsent bytes).
    pub in_flight_bytes: Option<u32>,
    /// The lowest RTT seen lately, microseconds (Linux `tcpi_min_rtt`, 4.9).
    pub min_rtt_us: Option<u32>,
    /// The smoothed RTT, microseconds (macOS keeps milliseconds).
    pub srtt_us: Option<u32>,
    /// The latest delivery-rate sample, bytes per second (Linux `tcpi_delivery_rate`, 4.9).
    pub delivery_rate: Option<u64>,
    /// Whether that sample was limited by the application having nothing to send, not by the path.
    pub delivery_rate_app_limited: Option<bool>,
    /// The congestion window, bytes.
    pub cwnd_bytes: Option<u64>,
    /// The send segment size, bytes.
    pub mss: Option<u32>,
    /// Segments retransmitted over the connection's life (Linux `tcpi_total_retrans`).
    pub retransmits: Option<u32>,
    /// Bytes retransmitted over the connection's life (Linux `tcpi_bytes_retrans`, 4.19).
    pub retransmitted_bytes: Option<u64>,
    /// The congestion control in use (Linux `TCP_CONGESTION`).
    pub congestion: Option<CongestionName>,
}

// ============================================================================== Unix (rustix)

#[cfg(unix)]
mod imp {
    use std::io::{IoSlice, IoSliceMut};
    use std::os::fd::{AsRawFd, OwnedFd};

    use rustix::io::Errno;
    use rustix::net::{AddressFamily, SocketAddr, SocketFlags, SocketType};

    use super::{CongestionError, CongestionName, Shutdown, TcpInfo};
    #[cfg(target_os = "linux")]
    use super::CongestionRefusal;
    use crate::driver::refused;
    use crate::error::RtError;
    use crate::netsys::{Family, Io};

    /// A non-blocking, close-on-exec TCP socket owned here (closed on drop, by `OwnedFd`).
    #[derive(Debug)]
    pub(crate) struct Stream {
        fd: OwnedFd,
    }

    /// The owned OS handle a stream moves as: a file descriptor.
    pub(crate) type OwnedStream = OwnedFd;

    impl Stream {
        /// The readiness handle a `crate::readiness` future registers (the raw fd).
        pub(crate) fn raw_id(&self) -> i32 {
            self.fd.as_raw_fd()
        }

        /// The OS handle, for a query the seam does not make (`crate::localsys`'s peer identity).
        pub(crate) fn raw_handle(&self) -> i32 {
            self.fd.as_raw_fd()
        }
    }

    /// The `Io` a failed non-blocking call maps to, or the refusal it is.
    fn classify<T>(call: &'static str, e: Errno) -> Result<Io<T>, RtError> {
        match e {
            Errno::AGAIN => Ok(Io::WouldBlock),
            Errno::INTR => Ok(Io::Interrupted),
            e => Err(refused(call, e)),
        }
    }

    /// Marks `fd` close-on-exec and non-blocking (a fresh socket, and each accepted one, which does not
    /// inherit non-blocking on macOS).
    fn prepare(fd: &OwnedFd) -> Result<(), RtError> {
        rustix::io::fcntl_setfd(fd, rustix::io::FdFlags::CLOEXEC)
            .map_err(|e| refused("fcntl(CLOEXEC)", e))?;
        rustix::io::ioctl_fionbio(fd, true).map_err(|e| refused("ioctl(FIONBIO)", e))
    }

    pub(crate) fn stream_socket(family: Family) -> Result<Stream, RtError> {
        let domain = match family {
            Family::V4 => AddressFamily::INET,
            Family::V6 => AddressFamily::INET6,
        };
        let fd = rustix::net::socket_with(domain, SocketType::STREAM, SocketFlags::empty(), None)
            .map_err(|e| refused("socket(STREAM)", e))?;
        prepare(&fd)?;
        if family == Family::V6 {
            rustix::net::sockopt::set_ipv6_v6only(&fd, true)
                .map_err(|e| refused("setsockopt(IPV6_V6ONLY)", e))?;
        }
        Ok(Stream { fd })
    }

    /// Adopts a stream socket the caller owns (accepted elsewhere, or inherited), making it non-blocking.
    pub(crate) fn adopt(fd: OwnedStream) -> Result<Stream, RtError> {
        let kind = rustix::net::sockopt::socket_type(&fd)
            .map_err(|e| refused("getsockopt(SO_TYPE)", e))?;
        if kind != SocketType::STREAM {
            return Err(refused("adopt(SO_TYPE)", Errno::PROTOTYPE));
        }
        prepare(&fd)?;
        Ok(Stream { fd })
    }

    pub(crate) fn into_owned(stream: Stream) -> OwnedStream {
        stream.fd
    }

    /// Lets every shard bind its own listener to one address, the kernel spreading connections among them
    /// (`SO_REUSEPORT`; on Linux by a hash of the connection's addresses, socket(7)).
    pub(crate) fn set_reuseport(stream: &Stream) -> Result<(), RtError> {
        rustix::net::sockopt::set_socket_reuseport(&stream.fd, true)
            .map_err(|e| refused("setsockopt(SO_REUSEPORT)", e))
    }

    pub(crate) fn bind(stream: &Stream, addr: SocketAddr) -> Result<(), RtError> {
        rustix::net::bind(&stream.fd, &addr).map_err(|e| refused("bind", e))
    }

    pub(crate) fn listen(stream: &Stream, backlog: i32) -> Result<(), RtError> {
        rustix::net::listen(&stream.fd, backlog).map_err(|e| refused("listen", e))
    }

    /// Takes one pending connection. A connection reset while it waited in the queue (`ECONNABORTED`) is
    /// nothing to take: read past, as an interrupted call.
    pub(crate) fn accept(stream: &Stream) -> Result<Io<Stream>, RtError> {
        match rustix::net::accept(&stream.fd) {
            Ok(fd) => {
                prepare(&fd)?;
                Ok(Io::Ready(Stream { fd }))
            }
            // A connection that died in the queue, and the errors accept(2) on Linux says to treat as
            // EAGAIN (a pending network error of the new socket, or a firewall's refusal): one client's
            // trouble, not the listener's, so it is read past (mantle's review, finding 5).
            Err(
                Errno::CONNABORTED
                | Errno::NETDOWN
                | Errno::PROTO
                | Errno::NOPROTOOPT
                | Errno::HOSTDOWN
                | Errno::HOSTUNREACH
                | Errno::OPNOTSUPP
                | Errno::NETUNREACH
                | Errno::PERM,
            ) => Ok(Io::Interrupted),
            #[cfg(target_os = "linux")]
            Err(Errno::NONET) => Ok(Io::Interrupted),
            Err(e) => classify("accept", e),
        }
    }

    /// Starts a connection: ready when it completed at once, `WouldBlock` while the handshake runs (its
    /// result is [`take_error`] once the socket is writable).
    pub(crate) fn connect(stream: &Stream, addr: SocketAddr) -> Result<Io<()>, RtError> {
        match rustix::net::connect(&stream.fd, &addr) {
            Ok(()) => Ok(Io::Ready(())),
            // A connect interrupted by a signal goes on in the background, as one in progress does.
            Err(Errno::INPROGRESS | Errno::INTR) => Ok(Io::WouldBlock),
            Err(e) => Err(refused("connect", e)),
        }
    }

    /// The socket's pending error (`SO_ERROR`), taken: how a background connect ended.
    pub(crate) fn take_error(stream: &Stream) -> Result<Option<i32>, RtError> {
        rustix::net::sockopt::socket_error(&stream.fd)
            .map(|outcome| outcome.err().map(Errno::raw_os_error))
            .map_err(|e| refused("getsockopt(SO_ERROR)", e))
    }

    pub(crate) fn read(stream: &Stream, buf: &mut [u8]) -> Result<Io<usize>, RtError> {
        match rustix::io::read(&stream.fd, buf) {
            Ok(n) => Ok(Io::Ready(n)),
            Err(e) => classify("read", e),
        }
    }

    pub(crate) fn read_vectored(
        stream: &Stream,
        bufs: &mut [IoSliceMut<'_>],
    ) -> Result<Io<usize>, RtError> {
        match rustix::io::readv(&stream.fd, bufs) {
            Ok(n) => Ok(Io::Ready(n)),
            Err(e) => classify("readv", e),
        }
    }

    /// Writes from `buf`. `MSG_NOSIGNAL` on Linux: a write to a peer that closed is `EPIPE`, never a
    /// `SIGPIPE` that ends the process (macOS sets `SO_NOSIGPIPE` on the socket instead, `set_options`).
    pub(crate) fn write(stream: &Stream, buf: &[u8]) -> Result<Io<usize>, RtError> {
        #[cfg(target_os = "linux")]
        let written = rustix::net::send(&stream.fd, buf, rustix::net::SendFlags::NOSIGNAL);
        #[cfg(not(target_os = "linux"))]
        let written = rustix::io::write(&stream.fd, buf);
        match written {
            Ok(n) => Ok(Io::Ready(n)),
            Err(e) => classify("write", e),
        }
    }

    /// Writes from `bufs` in one call. On Linux through `sendmsg` with `MSG_NOSIGNAL` (as [`write`]).
    pub(crate) fn write_vectored(
        stream: &Stream,
        bufs: &[IoSlice<'_>],
    ) -> Result<Io<usize>, RtError> {
        #[cfg(target_os = "linux")]
        let written = rustix::net::sendmsg(
            &stream.fd,
            bufs,
            &mut rustix::net::SendAncillaryBuffer::default(),
            rustix::net::SendFlags::NOSIGNAL,
        );
        #[cfg(not(target_os = "linux"))]
        let written = rustix::io::writev(&stream.fd, bufs);
        match written {
            Ok(n) => Ok(Io::Ready(n)),
            Err(e) => classify("writev", e),
        }
    }

    pub(crate) fn shutdown(stream: &Stream, how: Shutdown) -> Result<(), RtError> {
        let how = match how {
            Shutdown::Read => rustix::net::Shutdown::Read,
            Shutdown::Write => rustix::net::Shutdown::Write,
            Shutdown::Both => rustix::net::Shutdown::Both,
        };
        match rustix::net::shutdown(&stream.fd, how) {
            // Shutting a half the peer already reset is no failure of the caller's.
            Ok(()) | Err(Errno::NOTCONN) => Ok(()),
            Err(e) => Err(refused("shutdown", e)),
        }
    }

    /// The options every connected TCP stream carries: `TCP_NODELAY` (`crate::tcp`), and
    /// [`set_no_sigpipe`].
    pub(crate) fn set_options(stream: &Stream) -> Result<(), RtError> {
        rustix::net::sockopt::set_tcp_nodelay(&stream.fd, true)
            .map_err(|e| refused("setsockopt(TCP_NODELAY)", e))?;
        set_no_sigpipe(stream)
    }

    /// A write to a closed peer is `EPIPE`, never a `SIGPIPE`: macOS's `SO_NOSIGPIPE` on the socket (Linux
    /// passes `MSG_NOSIGNAL` per write, [`write`]). Every stream, TCP or local, carries it.
    pub(crate) fn set_no_sigpipe(stream: &Stream) -> Result<(), RtError> {
        #[cfg(target_os = "macos")]
        set_int(
            stream,
            libc::SOL_SOCKET,
            libc::SO_NOSIGPIPE,
            1,
            "setsockopt(SO_NOSIGPIPE)",
        )?;
        let _ = stream;
        Ok(())
    }

    /// Whether `TCP_NODELAY` is set, read back.
    pub(crate) fn nodelay(stream: &Stream) -> Result<bool, RtError> {
        rustix::net::sockopt::tcp_nodelay(&stream.fd)
            .map_err(|e| refused("getsockopt(TCP_NODELAY)", e))
    }

    /// Format: `TCP_NOTSENT_LOWAT`, 0x201 in XNU's `bsd/netinet/tcp.h`, which libc 0.2.189 does not declare
    /// for Apple targets; [`notsent_lowat`] reads it back, so a wrong value fails
    /// `the_stream_options_read_back`.
    #[cfg(target_os = "macos")]
    const TCP_NOTSENT_LOWAT: libc::c_int = 0x201;
    /// Format: `TCP_NOTSENT_LOWAT` (tcp(7), Linux 3.12), as libc declares it.
    #[cfg(target_os = "linux")]
    const TCP_NOTSENT_LOWAT: libc::c_int = libc::TCP_NOTSENT_LOWAT;

    /// Sets how few unsent bytes make the socket writable (`TCP_NOTSENT_LOWAT`, Linux 3.12 and macOS).
    /// Whether the platform offers it.
    pub(crate) fn set_notsent_lowat(stream: &Stream, bytes: u32) -> Result<bool, RtError> {
        #[cfg(any(target_os = "linux", target_os = "macos"))]
        {
            let value = libc::c_int::try_from(bytes).unwrap_or(libc::c_int::MAX);
            set_int(
                stream,
                libc::IPPROTO_TCP,
                TCP_NOTSENT_LOWAT,
                value,
                "setsockopt(TCP_NOTSENT_LOWAT)",
            )?;
            Ok(true)
        }
        #[cfg(not(any(target_os = "linux", target_os = "macos")))]
        {
            let _ = (stream, bytes);
            Ok(false)
        }
    }

    /// `TCP_NOTSENT_LOWAT` read back, where offered.
    pub(crate) fn notsent_lowat(stream: &Stream) -> Result<Option<u32>, RtError> {
        #[cfg(any(target_os = "linux", target_os = "macos"))]
        {
            let value = get_int(
                stream,
                libc::IPPROTO_TCP,
                TCP_NOTSENT_LOWAT,
                "getsockopt(TCP_NOTSENT_LOWAT)",
            )?;
            Ok(u32::try_from(value).ok())
        }
        #[cfg(not(any(target_os = "linux", target_os = "macos")))]
        {
            let _ = stream;
            Ok(None)
        }
    }

    /// Format: `TCP_RXT_CONNDROPTIME`, 0x80 in XNU's `bsd/netinet/tcp.h` ("time after which tcp
    /// retransmit timeouts cause connection drop", seconds), which libc 0.2.189 does not declare;
    /// [`user_timeout_ms`] reads it back.
    #[cfg(target_os = "macos")]
    const TCP_RXT_CONNDROPTIME: libc::c_int = 0x80;

    /// Format: milliseconds in a second.
    #[cfg(target_os = "macos")]
    const MS_PER_SECOND: u32 = 1_000;

    /// Sets how long sent data may stay unacknowledged before the connection is dropped: Linux's
    /// `TCP_USER_TIMEOUT` in milliseconds (RFC 5482), macOS's `TCP_RXT_CONNDROPTIME` in whole seconds,
    /// rounded up so the deadline is never shorter than asked. Whether the platform offers it.
    pub(crate) fn set_user_timeout(stream: &Stream, ms: u32) -> Result<bool, RtError> {
        #[cfg(target_os = "linux")]
        {
            rustix::net::sockopt::set_tcp_user_timeout(&stream.fd, ms)
                .map_err(|e| refused("setsockopt(TCP_USER_TIMEOUT)", e))?;
            Ok(true)
        }
        #[cfg(target_os = "macos")]
        {
            let seconds = ms.div_ceil(MS_PER_SECOND);
            let value = libc::c_int::try_from(seconds).unwrap_or(libc::c_int::MAX);
            set_int(
                stream,
                libc::IPPROTO_TCP,
                TCP_RXT_CONNDROPTIME,
                value,
                "setsockopt(TCP_RXT_CONNDROPTIME)",
            )?;
            Ok(true)
        }
        #[cfg(not(any(target_os = "linux", target_os = "macos")))]
        {
            let _ = (stream, ms);
            Ok(false)
        }
    }

    /// The deadline [`set_user_timeout`] set, in milliseconds (macOS: its whole seconds), where offered.
    pub(crate) fn user_timeout_ms(stream: &Stream) -> Result<Option<u32>, RtError> {
        #[cfg(target_os = "linux")]
        {
            rustix::net::sockopt::tcp_user_timeout(&stream.fd)
                .map(Some)
                .map_err(|e| refused("getsockopt(TCP_USER_TIMEOUT)", e))
        }
        #[cfg(target_os = "macos")]
        {
            let seconds = get_int(
                stream,
                libc::IPPROTO_TCP,
                TCP_RXT_CONNDROPTIME,
                "getsockopt(TCP_RXT_CONNDROPTIME)",
            )?;
            Ok(u32::try_from(seconds)
                .ok()
                .and_then(|s| s.checked_mul(MS_PER_SECOND)))
        }
        #[cfg(not(any(target_os = "linux", target_os = "macos")))]
        {
            let _ = stream;
            Ok(None)
        }
    }

    /// Picks the socket's congestion control (`TCP_CONGESTION`, Linux; a listener's choice is inherited by
    /// what it accepts, tcp(7)). Whether the platform offers it; a refusal names the reason, and the socket
    /// keeps the algorithm it had (bp-g3 step 1).
    pub(crate) fn set_congestion(
        stream: &Stream,
        name: CongestionName,
    ) -> Result<bool, CongestionError> {
        #[cfg(target_os = "linux")]
        {
            let bytes = name.as_bytes();
            let len = libc::socklen_t::try_from(bytes.len()).unwrap_or(libc::socklen_t::MAX);
            // SAFETY: the option takes a string of `len` bytes (no NUL needed, tcp(7)); the pointer and length
            // name `name`'s live inline bytes, and the descriptor is this socket's, open for the call.
            let outcome = unsafe {
                libc::setsockopt(
                    stream.fd.as_raw_fd(),
                    libc::IPPROTO_TCP,
                    libc::TCP_CONGESTION,
                    bytes.as_ptr().cast::<libc::c_void>(),
                    len,
                )
            };
            if outcome == 0 {
                return Ok(true);
            }
            let code = std::io::Error::last_os_error().raw_os_error();
            let reason = match code {
                Some(libc::EPERM) => CongestionRefusal::NotAllowed,
                Some(libc::ENOENT) => CongestionRefusal::NotLoaded,
                other => CongestionRefusal::Os(other),
            };
            Err(CongestionError {
                algorithm: name,
                reason,
            })
        }
        #[cfg(not(target_os = "linux"))]
        {
            let _ = (stream, name);
            Ok(false)
        }
    }

    /// The congestion control in use, where offered (Linux).
    pub(crate) fn congestion(stream: &Stream) -> Result<Option<CongestionName>, RtError> {
        #[cfg(target_os = "linux")]
        {
            let mut buf = [0u8; super::CONGESTION_NAME_BUF];
            let mut len = libc::socklen_t::try_from(buf.len()).unwrap_or(0);
            // SAFETY: the kernel writes at most `len` bytes of the name into `buf`, live and writable for the
            // call, and the actual length into `len`; the descriptor is this socket's.
            let outcome = unsafe {
                libc::getsockopt(
                    stream.fd.as_raw_fd(),
                    libc::IPPROTO_TCP,
                    libc::TCP_CONGESTION,
                    buf.as_mut_ptr().cast::<libc::c_void>(),
                    &raw mut len,
                )
            };
            if outcome != 0 {
                return Err(RtError::os("getsockopt(TCP_CONGESTION)"));
            }
            let got = buf.get(..usize::try_from(len).unwrap_or(0)).unwrap_or(&[]);
            Ok(CongestionName::from_kernel(got))
        }
        #[cfg(not(target_os = "linux"))]
        {
            let _ = stream;
            Ok(None)
        }
    }

    /// Sets the socket's send buffer (`SO_SNDBUF`; on Linux this turns its autotuning off, and the kernel
    /// doubles the value for its bookkeeping). Offered on every Unix.
    pub(crate) fn set_send_buffer(stream: &Stream, bytes: u32) -> Result<bool, RtError> {
        rustix::net::sockopt::set_socket_send_buffer_size(&stream.fd, to_usize(bytes))
            .map_err(|e| refused("setsockopt(SO_SNDBUF)", e))?;
        Ok(true)
    }

    /// `SO_SNDBUF` read back (Linux reports twice what was set).
    pub(crate) fn send_buffer(stream: &Stream) -> Result<Option<u32>, RtError> {
        rustix::net::sockopt::socket_send_buffer_size(&stream.fd)
            .map(|n| Some(u32::try_from(n).unwrap_or(u32::MAX)))
            .map_err(|e| refused("getsockopt(SO_SNDBUF)", e))
    }

    /// Sets the socket's receive buffer (`SO_RCVBUF`; on Linux this turns its autotuning off and isn't
    /// limited by `tcp_rmem`'s maximum). Offered on every Unix.
    pub(crate) fn set_recv_buffer(stream: &Stream, bytes: u32) -> Result<bool, RtError> {
        rustix::net::sockopt::set_socket_recv_buffer_size(&stream.fd, to_usize(bytes))
            .map_err(|e| refused("setsockopt(SO_RCVBUF)", e))?;
        Ok(true)
    }

    /// `SO_RCVBUF` read back (Linux reports twice what was set).
    pub(crate) fn recv_buffer(stream: &Stream) -> Result<Option<u32>, RtError> {
        rustix::net::sockopt::socket_recv_buffer_size(&stream.fd)
            .map(|n| Some(u32::try_from(n).unwrap_or(u32::MAX)))
            .map_err(|e| refused("getsockopt(SO_RCVBUF)", e))
    }

    /// A byte count as the buffer options take it.
    fn to_usize(bytes: u32) -> usize {
        usize::try_from(bytes).unwrap_or(usize::MAX)
    }

    /// Format: Linux's `struct tcp_info` (include/uapi/linux/tcp.h) through `tcpi_snd_wnd` (5.4), 232
    /// bytes. The kernel copies the prefix it has and says how long it was, so a field is read only when
    /// the returned length covers it.
    #[cfg(target_os = "linux")]
    #[repr(C)]
    #[derive(Default)]
    #[allow(dead_code, reason = "the kernel's layout: every field keeps its place, a few are read")]
    struct LinuxTcpInfo {
        state: u8,
        ca_state: u8,
        retransmits: u8,
        probes: u8,
        backoff: u8,
        options: u8,
        wscale: u8,
        /// Bit 0: `tcpi_delivery_rate_app_limited`; bits 1-2: `tcpi_fastopen_client_fail`.
        app_limited: u8,
        rto: u32,
        ato: u32,
        snd_mss: u32,
        rcv_mss: u32,
        unacked: u32,
        sacked: u32,
        lost: u32,
        retrans: u32,
        fackets: u32,
        last_data_sent: u32,
        last_ack_sent: u32,
        last_data_recv: u32,
        last_ack_recv: u32,
        pmtu: u32,
        rcv_ssthresh: u32,
        rtt: u32,
        rttvar: u32,
        snd_ssthresh: u32,
        snd_cwnd: u32,
        advmss: u32,
        reordering: u32,
        rcv_rtt: u32,
        rcv_space: u32,
        total_retrans: u32,
        pacing_rate: u64,
        max_pacing_rate: u64,
        bytes_acked: u64,
        bytes_received: u64,
        segs_out: u32,
        segs_in: u32,
        notsent_bytes: u32,
        min_rtt: u32,
        data_segs_in: u32,
        data_segs_out: u32,
        delivery_rate: u64,
        busy_time: u64,
        rwnd_limited: u64,
        sndbuf_limited: u64,
        delivered: u32,
        delivered_ce: u32,
        bytes_sent: u64,
        bytes_retrans: u64,
        dsack_dups: u32,
        reord_seen: u32,
        rcv_ooopack: u32,
        snd_wnd: u32,
    }

    /// Format: the layout above is the kernel's, 232 bytes (a field misplaced fails the build).
    #[cfg(target_os = "linux")]
    const _: [(); 232] = [(); size_of::<LinuxTcpInfo>()];

    /// Whether the kernel's prefix of `got` bytes covers the field of `size` bytes at `offset`.
    #[cfg(target_os = "linux")]
    fn covers(got: usize, offset: usize, size: usize) -> bool {
        offset.checked_add(size).is_some_and(|end| got >= end)
    }

    /// What the kernel says of the connection now (Linux `TCP_INFO` and `SIOCOUTQ`; macOS
    /// `TCP_CONNECTION_INFO`), where offered.
    pub(crate) fn tcp_info(stream: &Stream) -> Result<Option<TcpInfo>, RtError> {
        #[cfg(target_os = "linux")]
        {
            use core::mem::offset_of;
            let mut raw = LinuxTcpInfo::default();
            let mut len = libc::socklen_t::try_from(size_of::<LinuxTcpInfo>()).unwrap_or(0);
            // SAFETY: the kernel writes at most `len` bytes of its `tcp_info` into `raw`, a live local of that
            // size made only of integers (any bytes are a valid value), and the length it wrote into `len`.
            let outcome = unsafe {
                libc::getsockopt(
                    stream.fd.as_raw_fd(),
                    libc::IPPROTO_TCP,
                    libc::TCP_INFO,
                    (&raw mut raw).cast::<libc::c_void>(),
                    &raw mut len,
                )
            };
            if outcome != 0 {
                return Err(RtError::os("getsockopt(TCP_INFO)"));
            }
            let got = usize::try_from(len).unwrap_or(0);
            let u32s = size_of::<u32>();
            let u64s = size_of::<u64>();
            let unsent =
                covers(got, offset_of!(LinuxTcpInfo, notsent_bytes), u32s).then_some(raw.notsent_bytes);
            let mut queued: libc::c_int = 0;
            // SAFETY: `SIOCOUTQ` (`TIOCOUTQ`'s value on a TCP socket, tcp(7)) writes one `int`, the bytes not
            // yet acknowledged, into `queued`, a live local; the descriptor is this socket's.
            let outq =
                unsafe { libc::ioctl(stream.fd.as_raw_fd(), libc::TIOCOUTQ, &raw mut queued) };
            let in_flight = match (outq, unsent) {
                (0, Some(unsent)) => u32::try_from(queued).ok().map(|q| q.saturating_sub(unsent)),
                _ => None,
            };
            let rate = covers(got, offset_of!(LinuxTcpInfo, delivery_rate), u64s);
            Ok(Some(TcpInfo {
                unsent_bytes: unsent,
                in_flight_bytes: in_flight,
                min_rtt_us: covers(got, offset_of!(LinuxTcpInfo, min_rtt), u32s).then_some(raw.min_rtt),
                srtt_us: covers(got, offset_of!(LinuxTcpInfo, rtt), u32s).then_some(raw.rtt),
                delivery_rate: rate.then_some(raw.delivery_rate),
                delivery_rate_app_limited: rate.then_some(raw.app_limited & 1 == 1),
                cwnd_bytes: covers(got, offset_of!(LinuxTcpInfo, snd_cwnd), u32s)
                    .then(|| u64::from(raw.snd_cwnd).saturating_mul(u64::from(raw.snd_mss))),
                mss: covers(got, offset_of!(LinuxTcpInfo, snd_mss), u32s).then_some(raw.snd_mss),
                retransmits: covers(got, offset_of!(LinuxTcpInfo, total_retrans), u32s)
                    .then_some(raw.total_retrans),
                retransmitted_bytes: covers(got, offset_of!(LinuxTcpInfo, bytes_retrans), u64s)
                    .then_some(raw.bytes_retrans),
                congestion: congestion(stream)?,
            }))
        }
        #[cfg(target_os = "macos")]
        {
            // SAFETY: `tcp_connection_info` is integers only, so all-zero bytes are a valid value.
            let mut raw: libc::tcp_connection_info = unsafe { core::mem::zeroed() };
            let mut len =
                libc::socklen_t::try_from(size_of::<libc::tcp_connection_info>()).unwrap_or(0);
            // SAFETY: the kernel writes at most `len` bytes of its `tcp_connection_info` into `raw`, a live
            // local of that type, and the length it wrote into `len`; the descriptor is this socket's.
            let outcome = unsafe {
                libc::getsockopt(
                    stream.fd.as_raw_fd(),
                    libc::IPPROTO_TCP,
                    libc::TCP_CONNECTION_INFO,
                    (&raw mut raw).cast::<libc::c_void>(),
                    &raw mut len,
                )
            };
            if outcome != 0 {
                return Err(RtError::os("getsockopt(TCP_CONNECTION_INFO)"));
            }
            Ok(Some(TcpInfo {
                srtt_us: Some(raw.tcpi_srtt.saturating_mul(1_000)),
                cwnd_bytes: Some(u64::from(raw.tcpi_snd_cwnd)),
                mss: Some(raw.tcpi_maxseg),
                retransmitted_bytes: Some(raw.tcpi_txretransmitbytes),
                ..TcpInfo::default()
            }))
        }
        #[cfg(not(any(target_os = "linux", target_os = "macos")))]
        {
            let _ = stream;
            Ok(None)
        }
    }

    /// Sets one `int` option.
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    fn set_int(
        stream: &Stream,
        level: libc::c_int,
        option: libc::c_int,
        value: libc::c_int,
        call: &'static str,
    ) -> Result<(), RtError> {
        let len =
            libc::socklen_t::try_from(size_of::<libc::c_int>()).unwrap_or(libc::socklen_t::MAX);
        // SAFETY: the option takes an `int`; the pointer and length name one live local `c_int`, and the
        // descriptor is this socket's, open for the call.
        let outcome = unsafe {
            libc::setsockopt(
                stream.fd.as_raw_fd(),
                level,
                option,
                (&raw const value).cast::<libc::c_void>(),
                len,
            )
        };
        if outcome == 0 {
            Ok(())
        } else {
            Err(RtError::os(call))
        }
    }

    /// Reads one `int` option.
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    fn get_int(
        stream: &Stream,
        level: libc::c_int,
        option: libc::c_int,
        call: &'static str,
    ) -> Result<libc::c_int, RtError> {
        let mut value: libc::c_int = 0;
        let mut len =
            libc::socklen_t::try_from(size_of::<libc::c_int>()).unwrap_or(libc::socklen_t::MAX);
        // SAFETY: the option is an `int`; the out pointer and its length name one live local `c_int`, and
        // the descriptor is this socket's, open for the call.
        let outcome = unsafe {
            libc::getsockopt(
                stream.fd.as_raw_fd(),
                level,
                option,
                (&raw mut value).cast::<libc::c_void>(),
                &raw mut len,
            )
        };
        if outcome == 0 {
            Ok(value)
        } else {
            Err(RtError::os(call))
        }
    }

    pub(crate) fn local_addr(stream: &Stream) -> Result<SocketAddr, RtError> {
        let name = rustix::net::getsockname(&stream.fd).map_err(|e| refused("getsockname", e))?;
        SocketAddr::try_from(name).map_err(|_| refused("getsockname", Errno::AFNOSUPPORT))
    }

    pub(crate) fn peer_addr(stream: &Stream) -> Result<SocketAddr, RtError> {
        let name = rustix::net::getpeername(&stream.fd)
            .map_err(|e| refused("getpeername", e))?
            .ok_or(refused("getpeername", Errno::NOTCONN))?;
        SocketAddr::try_from(name).map_err(|_| refused("getpeername", Errno::AFNOSUPPORT))
    }
}

// ============================================================================== Windows (Winsock 2)

#[cfg(windows)]
mod imp {
    use std::io::{IoSlice, IoSliceMut};
    use std::os::windows::io::{FromRawSocket, IntoRawSocket, RawSocket};

    use windows_sys::Win32::Networking::WinSock::{
        AF_INET, AF_INET6, INVALID_SOCKET, IPPROTO_IPV6, IPPROTO_TCP, IPV6_V6ONLY, SD_BOTH,
        SD_RECEIVE, SD_SEND, SIO_TCP_INFO, SO_ERROR, SO_RCVBUF, SO_SNDBUF, SO_TYPE, SOCK_STREAM,
        SOCKADDR, SOCKADDR_STORAGE, SOCKET, SOCKET_ERROR, SOL_SOCKET, TCP_INFO_v0, TCP_MAXRTMS,
        TCP_NODELAY, WSABUF, WSAECONNABORTED, WSAECONNRESET, WSAEINPROGRESS, WSAEINTR, WSAENOTCONN,
        WSAEWOULDBLOCK, WSAGetLastError, WSAIoctl, WSARecv, WSASend,
        accept as ws_accept, bind as ws_bind, closesocket, connect as ws_connect, getpeername,
        getsockname, listen as ws_listen, recv, send, shutdown as ws_shutdown, socket as ws_socket,
    };

    use super::{CongestionError, CongestionName, Shutdown, TcpInfo};
    use crate::error::RtError;
    use crate::netsys::winsock::{
        ensure_started, from_sockaddr, int_option, last, length, set_int, set_nonblocking, sockaddr,
    };
    use crate::netsys::{Family, Io, SocketAddr};

    /// A non-blocking Winsock TCP socket owned here (closed on drop).
    #[derive(Debug)]
    pub(crate) struct Stream {
        socket: SOCKET,
    }

    impl Drop for Stream {
        fn drop(&mut self) {
            // SAFETY: our socket, created by `socket()` or `accept()`; closed exactly once here.
            unsafe { closesocket(self.socket) };
        }
    }

    /// The owned OS handle a stream moves as: a Winsock socket.
    pub(crate) type OwnedStream = std::os::windows::io::OwnedSocket;

    impl Stream {
        /// The readiness handle (see `crate::netsys`'s Windows `raw_id`).
        pub(crate) fn raw_id(&self) -> i32 {
            let low = u32::try_from(self.socket).unwrap_or(u32::MAX);
            i32::from_ne_bytes(low.to_ne_bytes())
        }

        /// The OS handle, for a query the seam does not make (`crate::localsys`'s peer identity).
        pub(crate) fn raw_handle(&self) -> SOCKET {
            self.socket
        }
    }

    /// The `Io` the last failed non-blocking call maps to, or the refusal it is.
    fn classify<T>(call: &'static str) -> Result<Io<T>, RtError> {
        // SAFETY: a pure query of thread-local last-error state.
        let code = unsafe { WSAGetLastError() };
        match code {
            WSAEWOULDBLOCK => Ok(Io::WouldBlock),
            WSAEINTR => Ok(Io::Interrupted),
            _ => Err(RtError::DriverRefused {
                call,
                code: Some(code),
            }),
        }
    }

    /// A buffer length as Winsock takes it: an `i32`, a longer buffer offered in part (the call reports
    /// what it moved).
    fn length_of(bytes: usize) -> i32 {
        i32::try_from(bytes).unwrap_or(i32::MAX)
    }

    /// A buffer count as `WSARecv`/`WSASend` take it.
    fn count_of(buffers: usize) -> u32 {
        u32::try_from(buffers).unwrap_or(u32::MAX)
    }

    pub(crate) fn stream_socket(family: Family) -> Result<Stream, RtError> {
        ensure_started()?;
        let domain = match family {
            Family::V4 => AF_INET,
            Family::V6 => AF_INET6,
        };
        // SAFETY: a plain socket creation; the result is checked against INVALID_SOCKET.
        let raw = unsafe { ws_socket(i32::from(domain), SOCK_STREAM, IPPROTO_TCP) };
        if raw == INVALID_SOCKET {
            return Err(last("socket(STREAM)"));
        }
        let stream = Stream { socket: raw };
        set_nonblocking(stream.socket)?;
        if family == Family::V6 {
            set_int(
                stream.socket,
                IPPROTO_IPV6,
                IPV6_V6ONLY,
                1,
                "setsockopt(IPV6_V6ONLY)",
            )?;
        }
        Ok(stream)
    }

    pub(crate) fn adopt(owned: OwnedStream) -> Result<Stream, RtError> {
        ensure_started()?;
        let Ok(raw) = SOCKET::try_from(owned.into_raw_socket()) else {
            return Err(RtError::DriverRefused {
                call: "adopt(SOCKET)",
                code: None,
            });
        };
        let stream = Stream { socket: raw };
        let kind = int_option(stream.socket, SOL_SOCKET, SO_TYPE, "getsockopt(SO_TYPE)")?;
        if kind != SOCK_STREAM {
            return Err(RtError::DriverRefused {
                call: "adopt(SO_TYPE)",
                code: Some(kind),
            });
        }
        set_nonblocking(stream.socket)?;
        Ok(stream)
    }

    pub(crate) fn into_owned(stream: Stream) -> OwnedStream {
        let stream = std::mem::ManuallyDrop::new(stream);
        let raw = RawSocket::try_from(stream.socket).unwrap_or(RawSocket::MAX);
        // SAFETY: `raw` is this seam's open socket, whose drop is suppressed above, so the `OwnedSocket`
        // becomes its one owner (see `crate::netsys`'s Windows `into_owned`).
        unsafe { OwnedStream::from_raw_socket(raw) }
    }

    pub(crate) fn bind(stream: &Stream, addr: SocketAddr) -> Result<(), RtError> {
        let (sa, len) = sockaddr(addr);
        // SAFETY: `sa` is a live socket address of `len` bytes, passed as the generic SOCKADDR.
        let rc = unsafe {
            ws_bind(
                stream.socket,
                std::ptr::addr_of!(sa).cast::<SOCKADDR>(),
                len,
            )
        };
        if rc == SOCKET_ERROR {
            Err(last("bind"))
        } else {
            Ok(())
        }
    }

    pub(crate) fn listen(stream: &Stream, backlog: i32) -> Result<(), RtError> {
        // SAFETY: a plain call on our socket.
        let rc = unsafe { ws_listen(stream.socket, backlog) };
        if rc == SOCKET_ERROR {
            Err(last("listen"))
        } else {
            Ok(())
        }
    }

    /// Takes one pending connection (see the Unix arm on `WSAECONNABORTED`).
    pub(crate) fn accept(stream: &Stream) -> Result<Io<Stream>, RtError> {
        // SAFETY: null address pointers ask for no peer address.
        let raw = unsafe { ws_accept(stream.socket, std::ptr::null_mut(), std::ptr::null_mut()) };
        if raw == INVALID_SOCKET {
            // A connection that died in the queue is one client's trouble, read past (finding 5).
            // SAFETY: a pure query of thread-local last-error state.
            let code = unsafe { WSAGetLastError() };
            if matches!(code, WSAECONNABORTED | WSAECONNRESET) {
                return Ok(Io::Interrupted);
            }
            return classify("accept");
        }
        let accepted = Stream { socket: raw };
        set_nonblocking(accepted.socket)?;
        Ok(Io::Ready(accepted))
    }

    /// Starts a connection: `WouldBlock` while the handshake runs (Winsock reports `WSAEWOULDBLOCK` for a
    /// non-blocking connect in progress).
    pub(crate) fn connect(stream: &Stream, addr: SocketAddr) -> Result<Io<()>, RtError> {
        let (sa, len) = sockaddr(addr);
        // SAFETY: `sa` is a live socket address of `len` bytes.
        let rc = unsafe {
            ws_connect(
                stream.socket,
                std::ptr::addr_of!(sa).cast::<SOCKADDR>(),
                len,
            )
        };
        if rc != SOCKET_ERROR {
            return Ok(Io::Ready(()));
        }
        // SAFETY: a pure query of thread-local last-error state.
        match unsafe { WSAGetLastError() } {
            WSAEWOULDBLOCK | WSAEINPROGRESS | WSAEINTR => Ok(Io::WouldBlock),
            _ => Err(last("connect")),
        }
    }

    pub(crate) fn take_error(stream: &Stream) -> Result<Option<i32>, RtError> {
        let code = int_option(stream.socket, SOL_SOCKET, SO_ERROR, "getsockopt(SO_ERROR)")?;
        Ok((code != 0).then_some(code))
    }

    pub(crate) fn read(stream: &Stream, buf: &mut [u8]) -> Result<Io<usize>, RtError> {
        // SAFETY: recv writes at most `len` bytes into `buf`, which is live and writable for the call.
        let rc = unsafe { recv(stream.socket, buf.as_mut_ptr(), length_of(buf.len()), 0) };
        if rc == SOCKET_ERROR {
            return classify("recv");
        }
        Ok(Io::Ready(usize::try_from(rc).unwrap_or(0)))
    }

    pub(crate) fn read_vectored(
        stream: &Stream,
        bufs: &mut [IoSliceMut<'_>],
    ) -> Result<Io<usize>, RtError> {
        let mut received: u32 = 0;
        let mut flags: u32 = 0;
        // SAFETY: `IoSliceMut` is guaranteed ABI-compatible with `WSABUF` on Windows (std::io::IoSliceMut),
        // so `bufs` is an array of `bufs.len()` live, writable `WSABUF`s; with no overlapped structure the
        // call completes now or fails `WSAEWOULDBLOCK`, writing within each buffer.
        let rc = unsafe {
            WSARecv(
                stream.socket,
                bufs.as_mut_ptr().cast::<WSABUF>(),
                count_of(bufs.len()),
                &raw mut received,
                &raw mut flags,
                std::ptr::null_mut(),
                None,
            )
        };
        if rc == SOCKET_ERROR {
            return classify("WSARecv");
        }
        Ok(Io::Ready(usize::try_from(received).unwrap_or(0)))
    }

    pub(crate) fn write(stream: &Stream, buf: &[u8]) -> Result<Io<usize>, RtError> {
        // SAFETY: send reads at most `len` bytes from `buf`, live for the call.
        let rc = unsafe { send(stream.socket, buf.as_ptr(), length_of(buf.len()), 0) };
        if rc == SOCKET_ERROR {
            return classify("send");
        }
        Ok(Io::Ready(usize::try_from(rc).unwrap_or(0)))
    }

    pub(crate) fn write_vectored(
        stream: &Stream,
        bufs: &[IoSlice<'_>],
    ) -> Result<Io<usize>, RtError> {
        let mut sent: u32 = 0;
        // SAFETY: `IoSlice` is guaranteed ABI-compatible with `WSABUF` on Windows (std::io::IoSlice); WSASend
        // only reads the buffers, live for the call; no overlapped structure, so it completes now or fails.
        let rc = unsafe {
            WSASend(
                stream.socket,
                bufs.as_ptr().cast::<WSABUF>(),
                count_of(bufs.len()),
                &raw mut sent,
                0,
                std::ptr::null_mut(),
                None,
            )
        };
        if rc == SOCKET_ERROR {
            return classify("WSASend");
        }
        Ok(Io::Ready(usize::try_from(sent).unwrap_or(0)))
    }

    pub(crate) fn shutdown(stream: &Stream, how: Shutdown) -> Result<(), RtError> {
        let how = match how {
            Shutdown::Read => SD_RECEIVE,
            Shutdown::Write => SD_SEND,
            Shutdown::Both => SD_BOTH,
        };
        // SAFETY: a plain call on our socket.
        let rc = unsafe { ws_shutdown(stream.socket, how) };
        // SAFETY: a pure query of thread-local last-error state.
        if rc == SOCKET_ERROR && unsafe { WSAGetLastError() } != WSAENOTCONN {
            return Err(last("shutdown"));
        }
        Ok(())
    }

    pub(crate) fn set_options(stream: &Stream) -> Result<(), RtError> {
        set_int(
            stream.socket,
            IPPROTO_TCP,
            TCP_NODELAY,
            1,
            "setsockopt(TCP_NODELAY)",
        )
    }

    /// Windows raises no `SIGPIPE`.
    pub(crate) fn set_no_sigpipe(_stream: &Stream) -> Result<(), RtError> {
        Ok(())
    }

    pub(crate) fn nodelay(stream: &Stream) -> Result<bool, RtError> {
        int_option(
            stream.socket,
            IPPROTO_TCP,
            TCP_NODELAY,
            "getsockopt(TCP_NODELAY)",
        )
        .map(|v| v != 0)
    }

    /// Windows has no `TCP_NOTSENT_LOWAT`; its idea of a send backlog is a query
    /// (`SIO_IDEAL_SEND_BACKLOG_QUERY`), not a writability threshold. Not offered.
    pub(crate) fn set_notsent_lowat(_stream: &Stream, _bytes: u32) -> Result<bool, RtError> {
        Ok(false)
    }

    pub(crate) fn notsent_lowat(_stream: &Stream) -> Result<Option<u32>, RtError> {
        Ok(None)
    }

    /// Windows picks its congestion control per system (`netsh` templates), not per socket. Not offered.
    pub(crate) fn set_congestion(
        _stream: &Stream,
        _name: CongestionName,
    ) -> Result<bool, CongestionError> {
        Ok(false)
    }

    pub(crate) fn congestion(_stream: &Stream) -> Result<Option<CongestionName>, RtError> {
        Ok(None)
    }

    /// `SO_SNDBUF` (Winsock): the bytes it buffers for sending; a set value turns its send-buffer
    /// autotuning off.
    pub(crate) fn set_send_buffer(stream: &Stream, bytes: u32) -> Result<bool, RtError> {
        let value = i32::try_from(bytes).unwrap_or(i32::MAX);
        set_int(stream.socket, SOL_SOCKET, SO_SNDBUF, value, "setsockopt(SO_SNDBUF)")?;
        Ok(true)
    }

    pub(crate) fn send_buffer(stream: &Stream) -> Result<Option<u32>, RtError> {
        let value = int_option(stream.socket, SOL_SOCKET, SO_SNDBUF, "getsockopt(SO_SNDBUF)")?;
        Ok(u32::try_from(value).ok())
    }

    /// `SO_RCVBUF` (Winsock).
    pub(crate) fn set_recv_buffer(stream: &Stream, bytes: u32) -> Result<bool, RtError> {
        let value = i32::try_from(bytes).unwrap_or(i32::MAX);
        set_int(stream.socket, SOL_SOCKET, SO_RCVBUF, value, "setsockopt(SO_RCVBUF)")?;
        Ok(true)
    }

    pub(crate) fn recv_buffer(stream: &Stream) -> Result<Option<u32>, RtError> {
        let value = int_option(stream.socket, SOL_SOCKET, SO_RCVBUF, "getsockopt(SO_RCVBUF)")?;
        Ok(u32::try_from(value).ok())
    }

    /// `SIO_TCP_INFO` at version 0 (Windows 10 1703): RTTs, bytes in flight, the window, the segment size
    /// and bytes retransmitted. No unsent count and no delivery rate.
    pub(crate) fn tcp_info(stream: &Stream) -> Result<Option<TcpInfo>, RtError> {
        let version: u32 = 0;
        let mut raw = TCP_INFO_v0::default();
        let mut returned: u32 = 0;
        let in_len = u32::try_from(size_of::<u32>()).unwrap_or(0);
        let out_len = u32::try_from(size_of::<TCP_INFO_v0>()).unwrap_or(0);
        // SAFETY: the call reads the one `u32` version at `version` and writes at most `out_len` bytes of
        // `TCP_INFO_v0` into `raw`, both live locals, and the count into `returned`; no overlapped structure,
        // so it completes now.
        let rc = unsafe {
            WSAIoctl(
                stream.socket,
                SIO_TCP_INFO,
                (&raw const version).cast::<core::ffi::c_void>(),
                in_len,
                (&raw mut raw).cast::<core::ffi::c_void>(),
                out_len,
                &raw mut returned,
                std::ptr::null_mut(),
                None,
            )
        };
        if rc == SOCKET_ERROR {
            return Err(last("WSAIoctl(SIO_TCP_INFO)"));
        }
        Ok(Some(TcpInfo {
            in_flight_bytes: Some(raw.BytesInFlight),
            min_rtt_us: Some(raw.MinRttUs),
            srtt_us: Some(raw.RttUs),
            cwnd_bytes: Some(u64::from(raw.Cwnd)),
            mss: Some(raw.Mss),
            retransmitted_bytes: Some(u64::from(raw.BytesRetrans)),
            ..TcpInfo::default()
        }))
    }

    /// `TCP_MAXRTMS` (Windows 10 1607): how long, in milliseconds, retransmissions go on before the
    /// connection is dropped, Linux's `TCP_USER_TIMEOUT` in effect.
    pub(crate) fn set_user_timeout(stream: &Stream, ms: u32) -> Result<bool, RtError> {
        let value = i32::try_from(ms).unwrap_or(i32::MAX);
        set_int(
            stream.socket,
            IPPROTO_TCP,
            TCP_MAXRTMS,
            value,
            "setsockopt(TCP_MAXRTMS)",
        )?;
        Ok(true)
    }

    pub(crate) fn user_timeout_ms(stream: &Stream) -> Result<Option<u32>, RtError> {
        let value = int_option(
            stream.socket,
            IPPROTO_TCP,
            TCP_MAXRTMS,
            "getsockopt(TCP_MAXRTMS)",
        )?;
        Ok(u32::try_from(value).ok())
    }

    pub(crate) fn local_addr(stream: &Stream) -> Result<SocketAddr, RtError> {
        name_of(stream, true)
    }

    pub(crate) fn peer_addr(stream: &Stream) -> Result<SocketAddr, RtError> {
        name_of(stream, false)
    }

    fn name_of(stream: &Stream, local: bool) -> Result<SocketAddr, RtError> {
        // SAFETY: an all-zero SOCKADDR_STORAGE is a valid empty address the call fills.
        let mut sa: SOCKADDR_STORAGE = unsafe { std::mem::zeroed() };
        let mut len = length::<SOCKADDR_STORAGE>();
        let out = std::ptr::addr_of_mut!(sa).cast::<SOCKADDR>();
        // SAFETY: the call writes up to `len` bytes into `sa` and the actual length into `len`.
        let rc = unsafe {
            if local {
                getsockname(stream.socket, out, &mut len)
            } else {
                getpeername(stream.socket, out, &mut len)
            }
        };
        let call = if local { "getsockname" } else { "getpeername" };
        if rc == SOCKET_ERROR {
            return Err(last(call));
        }
        from_sockaddr(&sa).ok_or(RtError::DriverRefused { call, code: None })
    }
}

#[cfg(unix)]
pub(crate) use imp::set_reuseport;
pub(crate) use imp::{
    OwnedStream, Stream, accept, adopt, bind, congestion, connect, into_owned, listen, local_addr,
    nodelay, notsent_lowat, peer_addr, read, read_vectored, recv_buffer, send_buffer,
    set_congestion, set_no_sigpipe, set_notsent_lowat, set_options, set_recv_buffer,
    set_send_buffer, set_user_timeout, shutdown, stream_socket, take_error, tcp_info,
    user_timeout_ms, write, write_vectored,
};

/// A stream socket of `addr`'s family, bound to it.
pub(crate) fn bound(addr: SocketAddr, reuseport: bool) -> Result<Stream, RtError> {
    let stream = stream_socket(Family::of(addr))?;
    #[cfg(unix)]
    if reuseport {
        set_reuseport(&stream)?;
    }
    #[cfg(not(unix))]
    if reuseport {
        return Err(RtError::BadConfig {
            what: "SO_REUSEPORT on a platform without it (Windows hands accepted sockets off instead)",
        });
    }
    bind(&stream, addr)?;
    Ok(stream)
}
