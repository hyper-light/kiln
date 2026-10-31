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
    owner: Option<ActivationOwner>,
}

/// U16: a route to the existing supervisor's owned process, not release approval or retirement proof.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct ActivationOwner {
    pub pid: u32,
    pub generation: u64,
}

/// U16: the existing installer's conventional mutable CLI endpoint, independent of
/// the selected runner image. Contract 1 supplies kiln's reach-MCP and Go helper CLIs.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct InstalledCli {
    contract: u32,
    system: bool,
    path: PathBuf,
}

pub const INSTALLED_CLI_CONTRACT: u32 = 1;
#[cfg(unix)]
pub(crate) const CONTEXT_WITHIN: Duration = Duration::from_secs(5);
#[cfg(unix)]
pub(crate) const CONTEXT_FRAME: usize = 64 << 10;
#[cfg(unix)]
pub(crate) const CONTEXT_MARKER: u8 = b'@';

impl InstalledCli {
    pub(crate) fn conventional(system: bool) -> PathBuf {
        if system { PathBuf::from("/usr/local/bin/kiln") }
        else { crate::os::home_dir().join(".local/bin/kiln") }
    }

    /// Called only by explicit installation after successful placement, in the
    /// qualified conventional role/layout. Custom layouts have no retention grant.
    pub(crate) fn from_install(path: PathBuf, system: bool) -> Result<Self, InstalledCliRefusal> {
        let value = Self { contract: INSTALLED_CLI_CONTRACT, system, path };
        value.validate()?;
        Ok(value)
    }

    pub(crate) fn validate(&self) -> Result<(), InstalledCliRefusal> {
        let path = self.path.to_str().ok_or(InstalledCliRefusal::Unsupported)?;
        if self.contract != INSTALLED_CLI_CONTRACT || path.len() > 4096
            || !self.path.is_absolute() || self.path != Self::conventional(self.system)
            || self.path.components().any(|c| matches!(c, std::path::Component::ParentDir | std::path::Component::CurDir))
        { return Err(InstalledCliRefusal::Unsupported); }
        Ok(())
    }

    pub fn path(&self) -> &Path { &self.path }
    pub(crate) fn argument(&self) -> Result<String> { Ok(serde_json::to_string(self)?) }
}

impl std::str::FromStr for InstalledCli {
    type Err = InstalledCliRefusal;
    fn from_str(value: &str) -> Result<Self, Self::Err> {
        if value.len() > 8192 { return Err(InstalledCliRefusal::Unsupported); }
        let context: Self = serde_json::from_str(value).map_err(|_| InstalledCliRefusal::Unsupported)?;
        context.validate()?;
        Ok(context)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InstalledCliRefusal { Unsupported, Busy }
impl std::fmt::Display for InstalledCliRefusal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Unsupported => "U16: installed helper context is unsupported for this process; explicit compatible conventional installation is required",
            Self::Busy => "U16: installed helper context is busy or its five-second deadline elapsed; no helper or takeover was started",
        })
    }
}
impl std::error::Error for InstalledCliRefusal {}

/// This read-only frame is bounded before growth. A delimiter belongs to the
/// 64 KiB cap; EOF, a second frame or trailing bytes in the read are refusal.
#[cfg(unix)]
pub(crate) async fn read_context_frame(stream: &hyper_rt::local::LocalStream) -> Result<serde_json::Value> {
    let mut bytes = Vec::new();
    let mut buffer = [0u8; 4096];
    loop {
        let remaining = CONTEXT_FRAME.checked_sub(bytes.len()).context("context frame too large")?;
        anyhow::ensure!(remaining > 0, "context frame exceeds 64 KiB");
        let take = remaining.min(buffer.len());
        let read = stream.read(buffer.get_mut(..take).context("context read bound")?).await?;
        anyhow::ensure!(read > 0, "context frame ended before its delimiter");
        let part = buffer.get(..read).context("context read length")?;
        if let Some(end) = part.iter().position(|byte| *byte == b'\n') {
            anyhow::ensure!(end + 1 == read, "trailing context frame bytes");
            bytes.extend_from_slice(part.get(..end).context("context delimiter bound")?);
            return Ok(serde_json::from_slice(&bytes)?);
        }
        bytes.extend_from_slice(part);
    }
}

#[cfg(unix)]
pub(crate) fn context_frame(value: &serde_json::Value) -> Result<Vec<u8>> {
    let mut bytes = serde_json::to_vec(value)?;
    anyhow::ensure!(bytes.len() < CONTEXT_FRAME, "context frame exceeds 64 KiB");
    bytes.push(b'\n');
    Ok(bytes)
}


impl Supervisor {
    pub fn from_env() -> Option<Self> {
        std::env::var_os(SUPERVISOR_ENV).map(|p| Self { path: PathBuf::from(p), owner: None }).filter(|s| s.path.exists())
    }

    /// Resolve only from this process's actual parent supervisor. The locator and
    /// generation from env never grant authority. One owned async stream and one
    /// overall deadline cover connect, kernel proof, bounded write/read and parse.
    pub async fn installed_cli() -> Result<InstalledCli, InstalledCliRefusal> {
        #[cfg(unix)]
        {
            let operation = async {
                let path = std::env::var_os(SUPERVISOR_ENV).map(PathBuf::from).ok_or(InstalledCliRefusal::Unsupported)?;
                if path.as_os_str().as_encoded_bytes().len() > 4096 { return Err(InstalledCliRefusal::Unsupported); }
                let generation = std::env::var("KILN_GENERATION").ok().and_then(|v| v.parse::<u64>().ok()).filter(|g| *g > 0).ok_or(InstalledCliRefusal::Unsupported)?;
                let owner = ActivationOwner { pid: std::process::id(), generation };
                let expected_parent=std::os::unix::process::parent_id();
                let stream = hyper_rt::local::LocalStream::connect(&path).await.map_err(|error| match error {
                    hyper_rt::error::RtError::WouldBlock { .. } => InstalledCliRefusal::Busy,
                    _ => InstalledCliRefusal::Unsupported,
                })?;
                let peer = stream.peer().map_err(|_| InstalledCliRefusal::Unsupported)?;
                let user = hyper_rt::local::current_user().map_err(|_| InstalledCliRefusal::Unsupported)?;
                if peer.user != user || peer.pid != Some(expected_parent)
                    || std::os::unix::process::parent_id()!=expected_parent {
                    return Err(InstalledCliRefusal::Unsupported);
                }
                // A context-only marker permits its own bounded server path without
                // imposing the 64 KiB cap on legacy mutating Start argv/env frames.
                let request = context_frame(&serde_json::json!({"op":"installed_cli","owner":owner})).map_err(|_| InstalledCliRefusal::Unsupported)?;
                stream.write_all(&[CONTEXT_MARKER]).await.map_err(|_| InstalledCliRefusal::Unsupported)?;
                stream.write_all(&request).await.map_err(|_| InstalledCliRefusal::Unsupported)?;
                let value = read_context_frame(&stream).await.map_err(|_| InstalledCliRefusal::Unsupported)?;
                if value.get("ok").and_then(serde_json::Value::as_bool) != Some(true) { return Err(InstalledCliRefusal::Unsupported); }
                let returned: ActivationOwner = serde_json::from_value(value.get("owner").cloned().ok_or(InstalledCliRefusal::Unsupported)?).map_err(|_| InstalledCliRefusal::Unsupported)?;
                if returned != owner { return Err(InstalledCliRefusal::Unsupported); }
                let context: InstalledCli = serde_json::from_value(value.get("installed_cli").cloned().ok_or(InstalledCliRefusal::Unsupported)?).map_err(|_| InstalledCliRefusal::Unsupported)?;
                context.validate()?;
                Ok(context)
            };
            crate::rt::timeout(CONTEXT_WITHIN,operation).await.map_err(|_| InstalledCliRefusal::Busy)?
        }
        #[cfg(not(unix))]
        { Err(InstalledCliRefusal::Unsupported) }
    }

    /// U16: advertise only a live explicit backend for this exact supervised process.
    /// An old supervisor refuses this read-only request; a path's existence proves nothing.
    pub fn for_activation() -> Result<Self> {
        let mut supervisor = Self::from_env().context(crate::updates::UpgradeRefusal::Backend)?;
        let generation = std::env::var("KILN_GENERATION").ok().and_then(|s| s.parse::<u64>().ok()).filter(|g| *g > 0)
            .context(crate::updates::UpgradeRefusal::Backend)?;
        supervisor.owner = Some(ActivationOwner { pid: std::process::id(), generation });
        supervisor.check_activation()?;
        Ok(supervisor)
    }

    pub fn check_activation(&self) -> Result<()> {
        let owner = self.owner.context(crate::updates::UpgradeRefusal::Backend)?;
        self.ask(&serde_json::json!({ "op": "activation", "owner": owner }))
            .context(crate::updates::UpgradeRefusal::Backend).map(|_| ())
    }

    /// Start `exe` with `args` beside this runner (it shares nothing with it but the machine).
    pub fn start(&self, exe: &Path, args: &[String], env: &[(String, String)]) -> Result<Green> {
        let v = self.ask(&serde_json::json!({ "op": "start", "exe": exe, "args": args, "env": env, "owner": self.owner }))?;
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
        s.set_write_timeout(Some(Duration::from_secs(30)))?;
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
            use crate::bluegreen::{ActivationOwner,InstalledCli,InstalledCliRefusal,Supervisor};
            use hyper_rt::local::LocalListener;
            const CASE:&str="KILN_U16_CONTEXT_FIXTURE";
            const TEST:&str="bluegreen::unix::tests::frames_carry_descriptors_both_ways";
            if let Ok(expected)=std::env::var(CASE) {
                if expected=="peer-server" {
                    let path=std::env::var_os(crate::bluegreen::SUPERVISOR_ENV).context("missing fixture socket")?;
                    return crate::rt::block_on(async move {
                        crate::rt::timeout(Duration::from_secs(10),async {
                            // Observe EOF even if the correctly refusing client has
                            // already closed. HyperRT may discard that accepted peer.
                            let listener=std::os::unix::net::UnixListener::bind(Path::new(&path))?;
                            listener.set_nonblocking(true)?;
                            let mut stream=loop {
                                match listener.accept() {
                                    Ok((stream,_))=>break stream,
                                    Err(err) if err.kind()==std::io::ErrorKind::WouldBlock=>crate::rt::sleep(Duration::from_millis(20)).await,
                                    Err(err)=>return Err(err.into()),
                                }
                            };
                            stream.set_nonblocking(true)?;
                            let mut byte=[0u8;1];
                            let n=loop {
                                match std::io::Read::read(&mut stream,&mut byte) {
                                    Ok(n)=>break n,
                                    Err(err) if err.kind()==std::io::ErrorKind::WouldBlock=>crate::rt::sleep(Duration::from_millis(20)).await,
                                    Err(err)=>return Err(err.into()),
                                }
                            };
                            assert_eq!(n,0,"wrong-parent client refuses before writing its request");
                            std::fs::write(std::env::var_os("KILN_U16_CONTEXT_RESULT").context("missing fixture receipt")?,b"peer-server")?;
                            anyhow::Ok(())
                        }).await?
                    });
                }
                let answer=crate::rt::block_on(async {anyhow::Ok(Supervisor::installed_cli().await)})?;
                if expected=="supported" {
                    assert_eq!(answer?.path(),InstalledCli::conventional(false));
                } else {
                    let want=if expected=="trickle" {InstalledCliRefusal::Busy} else {InstalledCliRefusal::Unsupported};
                    assert_eq!(answer.err(),Some(want),"actual bounded context consumer: {expected}");
                }
                std::fs::write(std::env::var_os("KILN_U16_CONTEXT_RESULT").context("missing fixture receipt")?,expected.as_bytes())?;
                return Ok(());
            }
            crate::rt::block_on(async move {
            // Real owned async connection, actual child/parent kernel credentials,
            // and one whole5s client budget; this is not a p99/timing benchmark.
            let context_dir=tempfile::Builder::new().prefix("ku16-").tempdir_in("/tmp")?;
            let executable=std::env::current_exe()?;
            for case in ["supported","trickle","oversize","eof","owner","generation"] {
                let path=context_dir.path().join(case);
                let listener=LocalListener::bind(&path,4)?;
                let receipt=context_dir.path().join(format!("{case}.result"));
                let mut child=std::process::Command::new(&executable).args(["--exact",TEST,"--nocapture"])
                    .stdout(std::process::Stdio::null())
                    .env(CASE,case).env("KILN_U16_CONTEXT_RESULT",&receipt).env(crate::bluegreen::SUPERVISOR_ENV,&path).env("KILN_GENERATION","71").spawn()?;
                let pid=child.id();
                let checked=crate::rt::timeout(Duration::from_secs(8),async {
                        let stream=listener.accept().await?;
                        let peer=stream.peer()?;
                        anyhow::ensure!(peer.pid==Some(pid),"fixture kernel peer PID mismatch");
                        anyhow::ensure!(peer.user==hyper_rt::local::current_user()?,"fixture kernel peer UID mismatch");
                        let mut marker=[0u8;1];anyhow::ensure!(stream.read(&mut marker).await?==1,"missing context marker");
                        anyhow::ensure!(marker==[crate::bluegreen::CONTEXT_MARKER],"wrong context marker");
                        let request=crate::bluegreen::read_context_frame(&stream).await?;
                        anyhow::ensure!(request.get("op").and_then(serde_json::Value::as_str)==Some("installed_cli"),"wrong context operation");
                        let mut owner:ActivationOwner=serde_json::from_value(request.get("owner").cloned().context("missing owner")?)?;
                        anyhow::ensure!(owner==ActivationOwner{pid,generation:71},"fixture original owner mismatch");
                        let mut retained=Some(stream);
                        if case=="eof" {drop(retained.take());}
                        else if case=="oversize" {
                            let oversized=vec![b'x';crate::bluegreen::CONTEXT_FRAME+1];
                            let _=retained.as_ref().context("missing over-cap stream")?.write_all(&oversized).await;
                        } else if case!="trickle" {
                            if case=="owner" {owner.pid=owner.pid.checked_add(1).context("fixture PID overflow")?;}
                            if case=="generation" {owner.generation+=1;}
                            let context=InstalledCli::from_install(InstalledCli::conventional(false),false)?;
                            let bytes=crate::bluegreen::context_frame(&serde_json::json!({"ok":true,"owner":owner,"installed_cli":context}))?;
                            retained.as_ref().context("missing fixture stream")?.write_all(&bytes).await?;
                        } else {retained.as_ref().context("missing trickle stream")?.write_all(b"{").await?;}
                        loop {
                            if let Some(status)=child.try_wait()? {return anyhow::Ok(status);}
                            // Each byte arrives well within a read timeout, but
                            // cannot restart the consumer's whole request budget.
                            if case=="trickle" && let Some(stream)=&retained {let _=stream.write_all(b" ").await;}
                            crate::rt::sleep(Duration::from_millis(100)).await;
                        }
                    }).await;
                // Every error path still owns and reaps only this fixture child.
                let _=child.kill();
                let reaped=crate::rt::timeout(Duration::from_secs(2),async {
                        loop {if let Some(status)=child.try_wait()? {return anyhow::Ok(status);}crate::rt::sleep(Duration::from_millis(20)).await;}
                    }).await;
                let status=reaped??;assert!(status.success(),"context fixture child failed: {case}");
                assert!(checked??.success(),"bounded context exchange failed: {case}");
                assert_eq!(std::fs::read(&receipt)?,case.as_bytes(),"selected child fixture really executed");
            }
            // A real sibling server has this user's UID but is not the client's
            // original parent. The client must refuse before sending its frame.
            let path=context_dir.path().join("peer");
            let server=std::process::Command::new(&executable).args(["--exact",TEST,"--nocapture"])
                    .stdout(std::process::Stdio::null())
                .env(CASE,"peer-server").env("KILN_U16_CONTEXT_RESULT",context_dir.path().join("peer-server.result")).env(crate::bluegreen::SUPERVISOR_ENV,&path).spawn()?;
            let mut children=vec![server];
            let checked=crate::rt::timeout(Duration::from_secs(8),async {
                    while !path.exists() {crate::rt::sleep(Duration::from_millis(20)).await;}
                    children.push(std::process::Command::new(&executable).args(["--exact",TEST,"--nocapture"])
                    .stdout(std::process::Stdio::null())
                        .env(CASE,"peer").env("KILN_U16_CONTEXT_RESULT",context_dir.path().join("peer.result")).env(crate::bluegreen::SUPERVISOR_ENV,&path).env("KILN_GENERATION","71").spawn()?);
                    loop {
                        let statuses=children.iter_mut().map(std::process::Child::try_wait).collect::<std::io::Result<Vec<_>>>()?;
                        if statuses.iter().all(Option::is_some) {
                            anyhow::ensure!(statuses.iter().flatten().all(std::process::ExitStatus::success),"wrong-parent fixture child failed");
                            return anyhow::Ok(());
                        }
                        crate::rt::sleep(Duration::from_millis(20)).await;
                    }
                }).await;
            // The handles remain outside the canceled future, including a client
            // spawned just before its deadline. Kill and reap both on every result.
            for child in &mut children {let _=child.kill();}
            let reaped=crate::rt::timeout(Duration::from_secs(2),async {
                    loop {
                        let statuses=children.iter_mut().map(std::process::Child::try_wait).collect::<std::io::Result<Vec<_>>>()?;
                        if statuses.iter().all(Option::is_some) {return anyhow::Ok(statuses);}
                        crate::rt::sleep(Duration::from_millis(20)).await;
                    }
                }).await;
            assert!(reaped??.iter().flatten().all(std::process::ExitStatus::success),"wrong-parent fixture failed");checked??;
            assert_eq!(std::fs::read(context_dir.path().join("peer-server.result"))?,b"peer-server");
            assert_eq!(std::fs::read(context_dir.path().join("peer.result"))?,b"peer");
                anyhow::Ok(())
            })?;
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
