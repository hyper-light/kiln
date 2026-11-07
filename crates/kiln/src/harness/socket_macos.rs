//! What a refused connection to a unix socket's path says on macOS. It refuses one where nothing
//! listens, and also where a listener's queue is full or the kernel couldn't make the connection
//! (xnu `bsd/kern/uipc_usrreq.c`, `unp_connect`: ECONNREFUSED where the path's vnode has no socket,
//! its socket doesn't listen, or `sonewconn` gives none, which `bsd/kern/uipc_socket2.c`'s
//! `sonewconn_internal` does once `so_qlen` reaches three halves of `so_qlimit`, or no socket can be
//! made). Linux tells a full queue apart (EAGAIN). So a refused probe here asks the kernel which of
//! this user's processes listen on the path (libproc's `PROC_PIDFDSOCKETINFO` for each of their
//! sockets, as lsof does), and the path is dead only where none does.

use std::collections::HashSet;
use std::os::unix::ffi::OsStrExt as _;

use nix::errno::Errno;
use nix::libc;

use super::Probe;

/// What a refused connection says ([`Refused::meaning`]): the paths this user's processes listen
/// on, looked up once for all the probes made with it (a sweep's).
pub(super) struct Refused(Option<Result<HashSet<Vec<u8>>, Errno>>);

impl Refused {
    /// Nothing looked up yet.
    pub(super) fn new() -> Self {
        Self(None)
    }

    /// A path a connection to was refused: a live listener's (its queue full) where a process of this
    /// user listens on it, else dead. Unknown when the processes couldn't be listed.
    pub(super) fn meaning(&mut self, path: &std::path::Path) -> Probe {
        match self.0.get_or_insert_with(listening) {
            Ok(paths) if paths.contains(path.as_os_str().as_bytes()) => Probe::Answers,
            Ok(_) => Probe::Dead,
            Err(e) => Probe::Unknown(*e),
        }
    }
}

/// `struct socket_fdinfo` (xnu `bsd/sys/proc_info.h`) as macOS lays it out, arm64 and x86_64 alike:
/// its size, and where the parts read here are in it (`psi.soi_family`, `psi.soi_options`,
/// `psi.soi_kind`, and the bound address, `psi.soi_proto.pri_un.unsi_addr.ua_sun.sun_path`, of
/// `sun_path`'s size). The kernel answers with the size of its own, and an answer of another size is
/// passed over.
const SOCKET_FDINFO: usize = 792;
const FAMILY: usize = 184;
const OPTIONS: usize = 188;
const KIND: usize = 256;
const SUN_PATH: usize = 282;
const SUN_PATH_LEN: usize = 104;
/// `struct proc_fdinfo`'s size: a descriptor and its kind.
const FDINFO: usize = std::mem::size_of::<libc::proc_fdinfo>();
/// The same header's `PROC_UID_ONLY`, `PROC_PIDFDSOCKETINFO` and `SOCKINFO_UN`.
const PROC_UID_ONLY: u32 = 4;
const PROC_PIDFDSOCKETINFO: libc::c_int = 3;
const SOCKINFO_UN: i32 = 3;

/// The paths this user's processes have unix sockets listening on, as the kernel reports them. A
/// process or a descriptor that goes while it's looked at, or one this process may not look into, is
/// passed over: an error only when the processes couldn't be listed.
#[allow(unsafe_code)]
fn listening() -> Result<HashSet<Vec<u8>>, Errno> {
    let uid = nix::unistd::geteuid().as_raw();
    // SAFETY (each libproc call here): its buffer is null with a size of 0 (asking for the size it
    // needs), or writable for the size given.
    let pids = filled(|buf, len| unsafe { libc::proc_listpids(PROC_UID_ONLY, uid, buf, len) })?;
    let socket = u32::try_from(libc::PROX_FDTYPE_SOCKET).ok();
    let mut paths = HashSet::new();
    for pid in pids.as_chunks::<{ std::mem::size_of::<libc::pid_t>() }>().0.iter().map(|c| libc::pid_t::from_ne_bytes(*c)).filter(|p| *p > 0) {
        let Ok(fds) = filled(|buf, len| unsafe { libc::proc_pidinfo(pid, libc::PROC_PIDLISTFDS, 0, buf, len) }) else { continue };
        for fd in fds.as_chunks::<FDINFO>().0 {
            let field = |at: usize| fd.get(at..at + 4).and_then(|b| <[u8; 4]>::try_from(b).ok());
            let (Some(fd), Some(kind)) = (field(std::mem::offset_of!(libc::proc_fdinfo, proc_fd)), field(std::mem::offset_of!(libc::proc_fdinfo, proc_fdtype))) else { continue };
            if Some(u32::from_ne_bytes(kind)) != socket {
                continue;
            }
            let mut info = [0u8; SOCKET_FDINFO];
            let n = unsafe { libc::proc_pidfdinfo(pid, libc::c_int::from_ne_bytes(fd), PROC_PIDFDSOCKETINFO, info.as_mut_ptr().cast(), libc::c_int::try_from(SOCKET_FDINFO).unwrap_or(0)) };
            if usize::try_from(n).ok() != Some(SOCKET_FDINFO) {
                continue;
            }
            let int = |at: usize| info.get(at..at + 4).and_then(|b| <[u8; 4]>::try_from(b).ok()).map(i32::from_ne_bytes);
            let options = info.get(OPTIONS..OPTIONS + 2).and_then(|b| <[u8; 2]>::try_from(b).ok()).map(i16::from_ne_bytes);
            let listens = options.is_some_and(|o| i32::from(o) & libc::SO_ACCEPTCONN != 0);
            if int(FAMILY) != Some(libc::AF_UNIX) || int(KIND) != Some(SOCKINFO_UN) || !listens {
                continue;
            }
            if let Some(bound) = info.get(SUN_PATH..SUN_PATH + SUN_PATH_LEN) {
                let path = bound.split(|b| *b == 0).next().unwrap_or_default();
                if !path.is_empty() {
                    paths.insert(path.to_vec());
                }
            }
        }
    }
    Ok(paths)
}

/// What a libproc call that fills a buffer gives, whole: asked its size first (a null buffer), then
/// given room for it, and twice the room while its answer fills it (what it lists grew meanwhile). Its
/// errors are libproc's: it answers 0 and sets errno.
fn filled(call: impl Fn(*mut libc::c_void, libc::c_int) -> libc::c_int) -> Result<Vec<u8>, Errno> {
    let size = call(std::ptr::null_mut(), 0);
    let mut room = usize::try_from(size).ok().filter(|s| *s > 0).ok_or_else(Errno::last)?;
    loop {
        let mut buf = vec![0u8; room];
        let given = usize::try_from(call(buf.as_mut_ptr().cast(), libc::c_int::try_from(room).unwrap_or(libc::c_int::MAX))).ok().filter(|n| *n > 0).ok_or_else(Errno::last)?;
        if given < room {
            buf.truncate(given);
            return Ok(buf);
        }
        room = room.saturating_mul(2);
    }
}
