//! Blue/green runner upgrades (docs/releases.md). The runner that is upgraded ("blue") never
//! replaces itself: its supervisor starts the new build beside it ("green") as another instance of
//! the same machine. Green must prove itself first, connected to the server and welcomed, within a
//! deadline; until then blue goes on with everything and new work still comes to it. Once green is
//! ready the server sends new work to it, and blue hands it every agent session and terminal over a
//! local socket, their processes still running (descriptors passed with `SCM_RIGHTS`, their state
//! written out whole). Blue finishes the jobs it is running, then exits, and the supervisor keeps
//! green. If green doesn't come up, or dies before it has taken over, it is stopped and blue goes
//! on as before: nothing is lost and the machine never goes offline.
//!
//! Here: the frames blue and green exchange, both ends of their socket, and the supervisor's
//! control socket as the runner asks it to start, promote or stop the new build
//! (`crate::supervise` serves it).

use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context as _, Result, bail};
use serde::{Deserialize, Serialize};

/// Where a supervised runner reaches its supervisor (set by `kiln supervise` for its child).
pub const SUPERVISOR_ENV: &str = "KILN_SUPERVISOR";
/// Where a new build's runner finds the runner it takes over from (set when it is started).
pub const TAKE_OVER_ENV: &str = "KILN_TAKE_OVER_FROM";
/// How long the new build has to connect to the server and say it is ready.
pub const READY_WITHIN: Duration = Duration::from_secs(180);

/// What blue and green say to each other, one frame at a time (length-prefixed JSON; the
/// descriptors a frame carries come with it). Externally tagged: an internally tagged enum is
/// read through serde's buffer, which can't read back maps keyed by numbers (a harness adapter's
/// state has them).
#[derive(Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Frame {
    /// green → blue: connected to the server and welcomed; it takes over now.
    Ready { pid: u32, instance: String },
    /// blue → green: an agent session, its harness process still running (descriptors: its stdin,
    /// stdout and stderr), or (`handed` none) to start again from its record.
    Session { id: String, record: Box<crate::harness::host::Saved>, handed: Option<Box<crate::harness::session::Handed>> },
    /// blue → green: a terminal, its shell still running. Descriptor: its PTY master.
    Terminal { session: String, command: Option<String>, cols: u16, rows: u16, replay: String, pid: u32 },
    /// blue → green: that's everything. Green serves the machine's local sockets (`kiln spawn`,
    /// `kiln run`) from now on: they are the same paths for every runner process of a machine.
    Done,
    /// blue → green, after `Done`: green is the runner now (its supervisor keeps it); it tells the
    /// server, which then sends it new work.
    Promoted,
    /// blue → green, after `Done`: a frame from the server about something handed over (or new
    /// that blue holds for green), which reached blue meanwhile (as the server sent it, base64).
    Forward { data: String },
    /// green → blue: taken over, or why not.
    Ack { id: String, ok: bool, error: Option<String> },
}

/// Where a runner whose own directory is `own_dir` waits for the new build to take over from it:
/// in the user's runtime directory (Unix socket paths are short), named after that directory.
pub fn handover_path(own_dir: &Path) -> PathBuf {
    let tag = crate::util::sha256_hex(own_dir.to_string_lossy().as_bytes());
    crate::harness::socket::runtime_path(&format!("handover-{}.sock", tag.get(..12).unwrap_or("0")))
}

/// The new build's runner, as its supervisor started it.
#[derive(Debug, Clone, Copy)]
pub struct Green {
    pub pid: u32,
}

/// The supervisor this runner runs under, when it can start a new build beside it.
#[derive(Debug, Clone)]
pub struct Supervisor {
    path: PathBuf,
}

impl Supervisor {
    pub fn from_env() -> Option<Self> {
        std::env::var_os(SUPERVISOR_ENV).map(|p| Self { path: PathBuf::from(p) }).filter(|s| s.path.exists())
    }

    /// Start `exe` with `args` beside this runner (it shares nothing with it but the machine).
    pub fn start(&self, exe: &Path, args: &[String], env: &[(String, String)]) -> Result<Green> {
        let v = self.ask(&serde_json::json!({ "op": "start", "exe": exe, "args": args, "env": env }))?;
        let pid = v.get("pid").and_then(serde_json::Value::as_u64).and_then(|p| u32::try_from(p).ok()).context("the supervisor didn't say which process it started")?;
        Ok(Green { pid })
    }

    /// From now on the new build is the runner the supervisor keeps (this one is about to end).
    pub fn promote(&self, green: Green) -> Result<()> {
        self.ask(&serde_json::json!({ "op": "promote", "pid": green.pid })).map(|_| ())
    }

    /// Stop the new build: it didn't come up.
    pub fn stop(&self, green: Green) -> Result<()> {
        self.ask(&serde_json::json!({ "op": "stop", "pid": green.pid })).map(|_| ())
    }

    /// Kill the new build outright (a rollback once it may hold what was handed over: nothing of
    /// it runs on to hang up a terminal or close a pipe this runner takes back).
    pub fn kill(&self, green: Green) -> Result<()> {
        self.ask(&serde_json::json!({ "op": "stop", "pid": green.pid, "kill": true })).map(|_| ())
    }

    #[cfg(unix)]
    fn ask(&self, req: &serde_json::Value) -> Result<serde_json::Value> {
        use std::io::{BufRead as _, Write as _};
        let mut s = std::os::unix::net::UnixStream::connect(&self.path).with_context(|| format!("reach the supervisor at {}", self.path.display()))?;
        s.set_read_timeout(Some(Duration::from_secs(30)))?;
        let mut line = req.to_string();
        line.push('\n');
        s.write_all(line.as_bytes())?;
        let mut answer = String::new();
        std::io::BufReader::new(s).read_line(&mut answer)?;
        let v: serde_json::Value = serde_json::from_str(&answer).context("the supervisor's answer")?;
        if v.get("ok").and_then(serde_json::Value::as_bool) != Some(true) {
            bail!("{}", v.get("error").and_then(serde_json::Value::as_str).unwrap_or("the supervisor refused"));
        }
        Ok(v)
    }

    #[cfg(not(unix))]
    fn ask(&self, _req: &serde_json::Value) -> Result<serde_json::Value> {
        bail!("blue/green upgrades need Unix sockets")
    }
}

/// Whether process `pid` still runs.
pub fn alive(pid: u32) -> bool {
    crate::os::pid_alive(pid)
}

#[cfg(unix)]
pub use unix::{Conn, Listener, connect};

#[cfg(unix)]
mod unix {
    use std::io::{IoSlice, IoSliceMut, Read as _, Write as _};
    use std::os::fd::{AsRawFd as _, FromRawFd as _, OwnedFd, RawFd};
    use std::os::unix::net::{UnixListener, UnixStream};

    use nix::sys::socket::{ControlMessage, ControlMessageOwned, MsgFlags, recvmsg, sendmsg};

    use std::time::Instant;

    use super::{Duration, Frame, Path, PathBuf, Result, bail};
    use anyhow::Context as _;

    /// Largest frame: a session's whole state, with room to spare.
    const MAX_FRAME: usize = 256 << 20;

    /// Blue's end, waiting for green.
    pub struct Listener {
        listener: UnixListener,
        path: PathBuf,
    }

    impl Listener {
        /// Listen at `path` (only this machine's user can connect).
        pub fn bind(path: &Path) -> Result<Self> {
            use std::os::unix::fs::PermissionsExt as _;
            if let Some(dir) = path.parent() {
                std::fs::create_dir_all(dir)?;
                let _ = std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700));
            }
            let _ = std::fs::remove_file(path);
            let listener = UnixListener::bind(path).with_context(|| format!("listen at {}", path.display()))?;
            std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
            listener.set_nonblocking(true)?;
            Ok(Self { listener, path: path.to_path_buf() })
        }

        pub fn path(&self) -> &Path {
            &self.path
        }

        /// Wait (blocking) for green (process `pid`) to connect and say it's ready, until
        /// `deadline`, as long as it runs. The connection to hand it everything over, and the
        /// instance it runs as.
        pub fn ready(&self, pid: u32, deadline: Instant) -> Result<(Conn, String)> {
            loop {
                match self.listener.accept() {
                    Ok((stream, _)) => {
                        stream.set_nonblocking(false)?;
                        stream.set_read_timeout(Some(Duration::from_secs(60)))?;
                        let conn = Conn { stream };
                        match conn.recv() {
                            Ok((Frame::Ready { pid: said, instance }, _)) if said == pid => return Ok((conn, instance)),
                            // Not the process started for this: not listened to.
                            Ok((Frame::Ready { pid: said, .. }, _)) => tracing::warn!(said, expected = pid, "a runner that isn't the new build tried to take over; ignored"),
                            Ok(_) => tracing::warn!("a connection to the handover socket didn't open by saying it's ready; ignored"),
                            Err(e) => tracing::warn!(error = %format!("{e:#}"), "a connection to the handover socket broke off; ignored"),
                        }
                    }
                    Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {}
                    Err(e) => return Err(e.into()),
                }
                if !super::alive(pid) {
                    bail!("the new runner (pid {pid}) exited before it was ready");
                }
                if Instant::now() >= deadline {
                    bail!("the new runner didn't say it was ready within {} s (it connects to the server, then to this runner)", super::READY_WITHIN.as_secs());
                }
                std::thread::sleep(Duration::from_millis(200));
            }
        }
    }

    impl Drop for Listener {
        fn drop(&mut self) {
            let _ = std::fs::remove_file(&self.path);
        }
    }

    /// Green's end: the runner it takes over from, at `path`. Reads wait as long as it takes:
    /// the connection ends when that runner does.
    pub fn connect(path: &Path) -> Result<Conn> {
        let stream = UnixStream::connect(path).with_context(|| format!("reach the runner being upgraded at {}", path.display()))?;
        Ok(Conn { stream })
    }

    /// One end of blue and green's connection. Blocking: used off the runtime.
    pub struct Conn {
        stream: UnixStream,
    }

    impl Conn {
        /// Send `frame`, with `fds` (they go with its first byte).
        pub fn send(&self, frame: &Frame, fds: &[RawFd]) -> Result<()> {
            let json = serde_json::to_vec(frame)?;
            anyhow::ensure!(json.len() <= MAX_FRAME, "a frame of {} bytes is too large", json.len());
            let len = u32::try_from(json.len())?.to_be_bytes();
            let iov = [IoSlice::new(&len)];
            let cmsgs = if fds.is_empty() { Vec::new() } else { vec![ControlMessage::ScmRights(fds)] };
            let sent = sendmsg::<()>(self.stream.as_raw_fd(), &iov, &cmsgs, MsgFlags::empty(), None)?;
            // A Unix stream sends a few bytes whole; the descriptors went with them.
            if let Some(rest) = len.get(sent..)
                && !rest.is_empty()
            {
                (&self.stream).write_all(rest)?;
            }
            (&self.stream).write_all(&json)?;
            Ok(())
        }

        /// The next frame, and the descriptors that came with it (theirs now).
        pub fn recv(&self) -> Result<(Frame, Vec<OwnedFd>)> {
            let mut len = [0u8; 4];
            let mut got = 0usize;
            let mut fds: Vec<OwnedFd> = Vec::new();
            while got < len.len() {
                let mut space = nix::cmsg_space!([RawFd; 8]);
                let (n, mut taken) = {
                    let Some(rest) = len.get_mut(got..) else { break };
                    let mut iov = [IoSliceMut::new(rest)];
                    let msg = recvmsg::<()>(self.stream.as_raw_fd(), &mut iov, Some(&mut space), MsgFlags::empty())?;
                    let mut taken = Vec::new();
                    for c in msg.cmsgs()? {
                        if let ControlMessageOwned::ScmRights(raw) = c {
                            taken.extend(raw);
                        }
                    }
                    (msg.bytes, taken)
                };
                for raw in taken.drain(..) {
                    #[allow(unsafe_code)]
                    // SAFETY: the kernel just installed `raw` in this process for this message
                    // (SCM_RIGHTS); nothing else here knows the number yet.
                    fds.push(unsafe { OwnedFd::from_raw_fd(raw) });
                }
                if n == 0 {
                    bail!("the other runner closed the connection");
                }
                got += n;
            }
            for fd in &fds {
                rustix::io::fcntl_setfd(fd, rustix::io::FdFlags::CLOEXEC)?;
            }
            let n = usize::try_from(u32::from_be_bytes(len))?;
            anyhow::ensure!(n <= MAX_FRAME, "a frame of {n} bytes is too large");
            let mut buf = vec![0u8; n];
            (&self.stream).read_exact(&mut buf)?;
            Ok((serde_json::from_slice(&buf).context("an unreadable frame")?, fds))
        }
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn frames_carry_descriptors_both_ways() -> anyhow::Result<()> {
            let (a, b) = UnixStream::pair()?;
            let (a, b) = (Conn { stream: a }, Conn { stream: b });
            // A pipe: its write end goes across, and still writes into the same pipe.
            let (r, w) = rustix::pipe::pipe()?;
            a.send(&Frame::Forward { data: "AAE=".into() }, &[w.as_raw_fd()])?;
            drop(w);
            let (frame, fds) = b.recv()?;
            assert!(matches!(frame, Frame::Forward { data } if data == "AAE="));
            assert_eq!(fds.len(), 1);
            rustix::io::write(fds.first().context("no fd")?, b"through")?;
            let mut buf = [0u8; 7];
            rustix::io::read(&r, &mut buf)?;
            assert_eq!(&buf, b"through");
            // A frame without any, and one larger than the socket holds (sent while it's read).
            let big = "x".repeat(3 << 20);
            let (frame, fds) = std::thread::scope(|s| {
                let sender = s.spawn(|| b.send(&Frame::Ack { id: big.clone(), ok: true, error: None }, &[]));
                let got = a.recv();
                let sent = sender.join().map_err(|_| anyhow::anyhow!("the sender panicked"));
                sent.and_then(|r| r).and(got)
            })?;
            assert!(matches!(frame, Frame::Ack { id, ok: true, .. } if id == big) && fds.is_empty());
            Ok(())
        }

    }
}
