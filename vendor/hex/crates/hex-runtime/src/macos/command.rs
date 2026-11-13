//! The programs Hex starts on macOS hold only the descriptors they are given, their stdio (decision 3;
//! CVE-2024-21626's class, CLAUDE.md §12). std's `Command` can't promise that here: its posix_spawn
//! passes no `POSIX_SPAWN_CLOEXEC_DEFAULT`, and macOS's pipe(2), socket(2), socketpair(2) and accept(2)
//! take no close-on-exec flag (std's own `io::pipe` marks its ends after making them), so a program std
//! starts inherits every descriptor another thread holds without the flag at that moment. This is the
//! part of std's `Command` Hex uses, on [`sys::spawn`] (`POSIX_SPAWN_CLOEXEC_DEFAULT`). As with std's,
//! the program runs in the caller's process group with the caller's environment (and the variables
//! the command adds), found as execvp(3) finds it, and is reaped only by the [`Child`] that owns it.
//! What a program prints is read a line at a time within a bound its caller derives
//! ([`Command::lines`]), so none of it accumulates here (CLAUDE.md §2).

use std::ffi::{CString, OsStr, OsString};
use std::io::{self, Read};
use std::os::fd::OwnedFd;
use std::os::unix::ffi::{OsStrExt, OsStringExt};
use std::os::unix::process::ExitStatusExt;
use std::process::ExitStatus;

use rustix::fs::OFlags;
use rustix::process::{Pid, Signal, WaitOptions};

use super::sys;

/// The caller's end of a program's stdin, stdout and stderr when piped.
pub type ChildStdin = io::PipeWriter;
pub type ChildStdout = io::PipeReader;
pub type ChildStderr = io::PipeReader;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Stream {
    /// The caller's own stream, or the null device when the caller has that stream closed.
    Inherit,
    Null,
    /// A new pipe; the caller holds the other end.
    Piped,
}

/// What one of a program's stdio streams is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Stdio(Stream);

impl Stdio {
    pub fn inherit() -> Self {
        Self(Stream::Inherit)
    }
    pub fn null() -> Self {
        Self(Stream::Null)
    }
    pub fn piped() -> Self {
        Self(Stream::Piped)
    }
}

/// A program to start: std's builder, for the calls Hex makes.
#[derive(Debug)]
pub struct Command {
    program: OsString,
    args: Vec<OsString>,
    env: Vec<(OsString, OsString)>,
    stdin: Option<Stream>,
    stdout: Option<Stream>,
    stderr: Option<Stream>,
}

impl Command {
    pub fn new(program: impl AsRef<OsStr>) -> Self {
        Self {
            program: program.as_ref().to_owned(),
            args: Vec::new(),
            env: Vec::new(),
            stdin: None,
            stdout: None,
            stderr: None,
        }
    }
    pub fn arg(&mut self, arg: impl AsRef<OsStr>) -> &mut Self {
        self.args.push(arg.as_ref().to_owned());
        self
    }
    pub fn args<I, S>(&mut self, args: I) -> &mut Self
    where
        I: IntoIterator<Item = S>,
        S: AsRef<OsStr>,
    {
        self.args
            .extend(args.into_iter().map(|arg| arg.as_ref().to_owned()));
        self
    }
    /// Sets `key` in the program's environment, over the caller's.
    pub fn env(&mut self, key: impl AsRef<OsStr>, value: impl AsRef<OsStr>) -> &mut Self {
        let key = key.as_ref().to_owned();
        self.env.retain(|(set, _)| *set != key);
        self.env.push((key, value.as_ref().to_owned()));
        self
    }
    pub fn stdin(&mut self, stdio: Stdio) -> &mut Self {
        self.stdin = Some(stdio.0);
        self
    }
    pub fn stdout(&mut self, stdio: Stdio) -> &mut Self {
        self.stdout = Some(stdio.0);
        self
    }
    pub fn stderr(&mut self, stdio: Stdio) -> &mut Self {
        self.stderr = Some(stdio.0);
        self
    }

    /// Starts the program; a stream not set is the caller's own, as std's `spawn`.
    pub fn spawn(&mut self) -> io::Result<Child> {
        self.start([Stream::Inherit; 3])
    }

    /// Runs the program to its end and returns its status; a stream not set is the caller's own, as
    /// std's `status`.
    pub fn status(&mut self) -> io::Result<ExitStatus> {
        self.start([Stream::Inherit; 3])?.wait()
    }

    /// Runs the program to its end, handing `each` every line of its stdout as it comes (its newline
    /// dropped), so nothing it prints accumulates here: a line holds at most `line_max` bytes, and one
    /// past that ends the program and is refused, `Exhausted` with `what`. A failure of `each` ends it
    /// too. stdin not set is the null device and stderr not set the caller's own; stdout is the pipe
    /// read here.
    pub fn lines(
        &mut self,
        what: &'static str,
        line_max: usize,
        mut each: impl FnMut(&[u8]) -> io::Result<()>,
    ) -> io::Result<ExitStatus> {
        self.stdout = Some(Stream::Piped);
        let mut child = self.start([Stream::Null, Stream::Piped, Stream::Inherit])?;
        let read = match child.stdout.take() {
            Some(stdout) => split(stdout, what, line_max, &mut each),
            None => Err(io::Error::other("the program's stdout isn't piped")),
        };
        // A program refused mid-output may still be writing: it ends here, and is reaped either way.
        let ended = match &read {
            Ok(()) => None,
            Err(_) => child.kill().err(),
        };
        let status = child.wait()?;
        match (read, ended) {
            (Ok(()), _) => Ok(status),
            (Err(error), None) => Err(error),
            (Err(error), Some(kill)) => Err(io::Error::new(
                error.kind(),
                format!("{error}; ending the program: {kill}"),
            )),
        }
    }

    fn start(&self, unset: [Stream; 3]) -> io::Result<Child> {
        let mut argv = Vec::new();
        argv.try_reserve_exact(self.args.len().saturating_add(1))
            .map_err(io::Error::other)?;
        argv.push(c_string(self.program.as_bytes())?);
        for arg in &self.args {
            argv.push(c_string(arg.as_bytes())?);
        }
        let mut environment: Vec<(OsString, OsString)> = std::env::vars_os()
            .filter(|(key, _)| self.env.iter().all(|(set, _)| set != key))
            .collect();
        environment.extend(self.env.iter().cloned());
        let mut envp = Vec::new();
        envp.try_reserve_exact(environment.len())
            .map_err(io::Error::other)?;
        for (key, value) in &environment {
            let mut entry = key.clone().into_vec();
            entry.push(b'=');
            entry.extend_from_slice(value.as_bytes());
            envp.push(c_string(&entry)?);
        }
        let path = environment
            .iter()
            .find(|(key, _)| key.as_bytes() == b"PATH")
            .map(|(_, value)| value.as_os_str());
        let [unset_in, unset_out, unset_err] = unset;
        let (stdin, ours_in) = source(self.stdin.unwrap_or(unset_in))?;
        let (stdout, ours_out) = sink(self.stdout.unwrap_or(unset_out), rustix::stdio::stdout())?;
        let (stderr, ours_err) = sink(self.stderr.unwrap_or(unset_err), rustix::stdio::stderr())?;
        let spawn = sys::Spawn {
            paths: paths(&self.program, path)?,
            argv,
            envp,
            cwd: None,
            stdio: [stdin, stdout, stderr],
            report: None,
            lifeline: None,
            jetsam_mib: None,
        };
        let group = rustix::process::getpgrp().as_raw_nonzero().get();
        let pid = sys::spawn(&spawn, group, sys::Clamp::Unclamped)?;
        // The program's ends close here; it holds its own copies.
        drop(spawn);
        Ok(Child {
            pid: Pid::from_raw(pid)
                .ok_or_else(|| io::Error::other("posix_spawn's pid isn't one"))?,
            status: None,
            stdin: ours_in,
            stdout: ours_out,
            stderr: ours_err,
        })
    }
}

/// A started program, reaped only by its owner: once reaped it is never signalled, as its pid may be
/// another process's by then (std's rule).
#[derive(Debug)]
pub struct Child {
    pid: Pid,
    status: Option<ExitStatus>,
    pub stdin: Option<ChildStdin>,
    pub stdout: Option<ChildStdout>,
    pub stderr: Option<ChildStderr>,
}

impl Child {
    pub fn id(&self) -> u32 {
        self.pid.as_raw_nonzero().get().unsigned_abs()
    }

    /// Its status if it has ended (reaping it then), else `None`.
    pub fn try_wait(&mut self) -> io::Result<Option<ExitStatus>> {
        if self.status.is_some() {
            return Ok(self.status);
        }
        Ok(
            rustix::process::waitpid(Some(self.pid), WaitOptions::NOHANG)?
                .map(|(_, status)| *self.status.insert(ExitStatus::from_raw(status.as_raw()))),
        )
    }

    /// Ends it with SIGKILL; one already reaped is left alone.
    pub fn kill(&mut self) -> io::Result<()> {
        if self.status.is_some() {
            return Ok(());
        }
        rustix::process::kill_process(self.pid, Signal::KILL).map_err(io::Error::from)
    }

    /// Waits for its end and reaps it, its stdin closed first so a program reading it can end (std's).
    pub fn wait(&mut self) -> io::Result<ExitStatus> {
        drop(self.stdin.take());
        if let Some(status) = self.status {
            return Ok(status);
        }
        loop {
            match rustix::process::waitpid(Some(self.pid), WaitOptions::empty()) {
                Ok(Some((_, status))) => {
                    return Ok(*self.status.insert(ExitStatus::from_raw(status.as_raw())));
                }
                Ok(None) | Err(rustix::io::Errno::INTR) => {}
                Err(error) => return Err(error.into()),
            }
        }
    }
}

fn c_string(bytes: &[u8]) -> io::Result<CString> {
    CString::new(bytes).map_err(|_| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            "a program's argument or environment holds a NUL byte",
        )
    })
}

/// The paths the program is executed from, in order, as execvp(3) tries them: its name when it holds a
/// `/`, else that name in each folder of `PATH`, or of `/usr/bin:/bin` when there is none.
fn paths(program: &OsStr, path: Option<&OsStr>) -> io::Result<Vec<CString>> {
    let name = program.as_bytes();
    if name.contains(&b'/') {
        return Ok(vec![c_string(name)?]);
    }
    path.map_or(b"/usr/bin:/bin".as_slice(), OsStrExt::as_bytes)
        .split(|byte| *byte == b':')
        .map(|folder| {
            let mut full = if folder.is_empty() {
                b".".to_vec()
            } else {
                folder.to_vec()
            };
            full.push(b'/');
            full.extend_from_slice(name);
            c_string(&full)
        })
        .collect()
}

/// The program's stdin and the caller's end of it.
fn source(stream: Stream) -> io::Result<(OwnedFd, Option<ChildStdin>)> {
    match stream {
        Stream::Inherit => inherited(rustix::stdio::stdin(), OFlags::RDONLY).map(|fd| (fd, None)),
        Stream::Null => Ok((super::null(OFlags::RDONLY).map_err(refused)?, None)),
        Stream::Piped => {
            let (reader, writer) = io::pipe()?;
            Ok((OwnedFd::from(reader), Some(writer)))
        }
    }
}

/// The program's stdout or stderr and the caller's end of it.
fn sink(
    stream: Stream,
    own: std::os::fd::BorrowedFd<'_>,
) -> io::Result<(OwnedFd, Option<io::PipeReader>)> {
    match stream {
        Stream::Inherit => inherited(own, OFlags::WRONLY).map(|fd| (fd, None)),
        Stream::Null => Ok((super::null(OFlags::WRONLY).map_err(refused)?, None)),
        Stream::Piped => {
            let (reader, writer) = io::pipe()?;
            Ok((OwnedFd::from(writer), Some(reader)))
        }
    }
}

/// A copy of the caller's own stream for the program, or the null device when the caller has it closed
/// (so the program's next open never lands on its stdio).
fn inherited(own: std::os::fd::BorrowedFd<'_>, null: OFlags) -> io::Result<OwnedFd> {
    match rustix::io::fcntl_dupfd_cloexec(own, 3) {
        Ok(copy) => Ok(copy),
        Err(rustix::io::Errno::BADF) => super::null(null).map_err(refused),
        Err(error) => Err(error.into()),
    }
}

fn refused(refusal: hex_api::Refusal) -> io::Error {
    io::Error::other(refusal.to_string())
}

/// Reads `stdout` to its end, handing `each` every line as it completes: one line and one read are
/// held, each at most `line_max` bytes (a read holds one line and its newline), and a line past that
/// is refused, `Exhausted` with `what`.
fn split(
    mut stdout: io::PipeReader,
    what: &'static str,
    line_max: usize,
    each: &mut impl FnMut(&[u8]) -> io::Result<()>,
) -> io::Result<()> {
    let exhausted = || {
        io::Error::other(hex_api::Refusal::Exhausted {
            what,
            bound: line_max,
        })
    };
    let room = line_max.checked_add(1).ok_or_else(exhausted)?;
    let mut line = Vec::new();
    line.try_reserve_exact(line_max).map_err(|_| exhausted())?;
    let mut chunk = Vec::new();
    chunk.try_reserve_exact(room).map_err(|_| exhausted())?;
    chunk.resize(room, 0u8);
    loop {
        let count = match stdout.read(&mut chunk) {
            Ok(0) => break,
            Ok(count) => count,
            Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
            Err(error) => return Err(error),
        };
        let read = chunk
            .get(..count)
            .ok_or_else(|| io::Error::other("a read past its buffer"))?;
        for piece in read.split_inclusive(|byte| *byte == b'\n') {
            let (body, whole) = match piece.strip_suffix(b"\n") {
                Some(body) => (body, true),
                None => (piece, false),
            };
            if line
                .len()
                .checked_add(body.len())
                .is_none_or(|length| length > line_max)
            {
                return Err(exhausted());
            }
            line.extend_from_slice(body);
            if whole {
                each(&line)?;
                line.clear();
            }
        }
    }
    if !line.is_empty() {
        each(&line)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::macos::directory::tests::{held, probe_args};
    use std::io::{BufRead, BufReader, Write};

    /// AC-T7.4.1 (CLAUDE.md §12): a program started with its stdio piped, as the scheduler's ssh
    /// tunnel starts one, holds only its stdio. Do: hold a descriptor without close-on-exec (a fresh
    /// pipe's end before its fcntl), and spawn this binary's descriptor probe with stdin and stdout
    /// piped and stderr inherited. Expect: it holds 0, 1 and 2, and nothing else.
    #[test]
    fn a_piped_program_holds_only_its_stdio() {
        let (reader, _writer) = io::pipe().unwrap();
        let _open = rustix::io::dup(&reader).unwrap();
        let mut child = Command::new(std::env::current_exe().unwrap())
            .args(probe_args())
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()
            .unwrap();
        let mut printed = String::new();
        child
            .stdout
            .take()
            .unwrap()
            .read_to_string(&mut printed)
            .unwrap();
        assert!(child.wait().unwrap().success());
        assert_eq!(held(&printed), [0, 1, 2]);
    }

    /// AC-T7.4.1 (LB03: a child is owned, never a guessed pid): a started program is reaped once and
    /// never signalled after. Do: start `cat` (found on `PATH`, as `ssh` is) with stdin and stdout
    /// piped, see it echo, kill it and wait. Expect: ended by SIGKILL, that status kept for try_wait
    /// and wait, and a kill after it no signal.
    #[test]
    fn a_child_is_reaped_once_and_never_signalled_after() {
        let mut child = Command::new("cat")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .spawn()
            .unwrap();
        child.stdin.as_mut().unwrap().write_all(b"x\n").unwrap();
        let mut line = String::new();
        BufReader::new(child.stdout.as_mut().unwrap())
            .read_line(&mut line)
            .unwrap();
        assert_eq!(line, "x\n");
        assert!(child.try_wait().unwrap().is_none());
        child.kill().unwrap();
        let status = child.wait().unwrap();
        assert_eq!(status.signal(), Some(libc::SIGKILL));
        assert_eq!(child.try_wait().unwrap(), Some(status));
        assert_eq!(child.wait().unwrap(), status);
        child.kill().unwrap();
    }

    /// AC-T7.4.1 (CLAUDE.md §2): `lines` hands on each line as it comes, holding one. Do: run `seq`
    /// for more lines than a pipe holds (100,000 of at most 6 bytes) with a bound of 6. Expect: every
    /// line in order, and its status.
    #[test]
    fn lines_hands_on_each_line_as_it_comes() {
        let mut next = 1u32;
        let status = Command::new("seq")
            .args(["1", "100000"])
            .lines("bytes in a line seq prints", 6, |line| {
                assert_eq!(line, next.to_string().as_bytes());
                next += 1;
                Ok(())
            })
            .unwrap();
        assert!(status.success());
        assert_eq!(next, 100_001);
    }

    /// AC-T7.4.1: a reader that refuses a line ends the program. Do: read `yes`, which never ends,
    /// and refuse its first line. Expect: that refusal, with the program ended and reaped.
    #[test]
    fn a_refused_line_ends_the_program() {
        let refused = Command::new("yes")
            .lines("bytes in a line yes prints", 2, |_| {
                Err(io::Error::other("enough"))
            })
            .unwrap_err();
        assert_eq!(refused.to_string(), "enough");
    }

    /// AC-T7.4.1: the machine's model is the kernel's `hw.model`, read with no program run. Do: read
    /// it, and run `sysctl -n hw.model`. Expect: the same model.
    #[test]
    fn the_machine_model_is_hw_model() {
        let mut printed = Vec::new();
        let status = Command::new("/usr/sbin/sysctl")
            .args(["-n", "hw.model"])
            .lines("bytes in a line sysctl prints", 256, |line| {
                printed.push(String::from_utf8(line.to_vec()).unwrap());
                Ok(())
            })
            .unwrap();
        assert!(status.success());
        assert_eq!(printed, [crate::macos::machine_model().unwrap()]);
    }
}
