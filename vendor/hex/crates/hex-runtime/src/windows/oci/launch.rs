//! A program started in an `oci` box (docs/design/runtime.md, decision "the Windows `oci` class is an HCS
//! silo holding a Hex job"). HCS can't start a process suspended, so the node has HCS start Hex's own
//! launcher in the container and the launcher starts the member suspended inside the box's job, as a
//! `process` box's member is started, then resumes it. The node gives the launcher the job (to assign a
//! process, nothing more) and the caller's three standard handles by `DuplicateHandle`, and one bounded
//! request on its stdin. The launcher replies with the member's pid on its stdout and holds the member's
//! handle until the node, having opened the member itself, closes the launcher's stdin; so the pid is never
//! reused in between. The node then waits on the member directly (`super::super::proc`), and the member
//! reads and writes the caller's pipes with no copier between. The wire is `hex_core::hcs`'s.

use std::io::{BufRead, BufReader, Read, Write};
use std::os::windows::io::{AsHandle, AsRawHandle, OwnedHandle};
use std::sync::mpsc;
use std::time::Duration;

use windows_sys::Win32::Foundation::HANDLE;

use hex_api::Refusal;
use hex_core::hcs::{self, Launch, Launched, MAX_WIRE, Process};

use super::sys::{self, HcsFailure, System};
use crate::windows::process::JobBox;
use crate::windows::sys::{Handle, Spawn};

/// How long the node waits for HCS to start the launcher and for the launcher's reply: hcsshim's bound on
/// an HCS create or start (4 minutes, `hcsshim/internal/timeout/timeout.go:11,21`), a bound on a wait for a
/// fact, never a guess at how long it takes (CLAUDE.md §4).
pub const PATIENCE: Duration = Duration::from_secs(240);

/// The exit code the node ends a launcher with that never replied.
const UNANSWERED: u32 = 0xE048_0100;

fn os(op: &'static str, e: impl std::fmt::Display) -> Refusal {
    Refusal::os(op, e)
}

fn hcs_os(op: &'static str, f: HcsFailure) -> Refusal {
    Refusal::Os {
        op,
        why: match f.message {
            Some(m) => format!("HCS {:#010x}: {m}", f.code),
            None => format!("HCS {:#010x}", f.code),
        },
    }
}

/// Starts `process` in `job` inside `system`, its standard streams `stdio`, through Hex's launcher run as
/// `user`. Returns the member's own handle (to wait on) and its pid.
pub fn start(
    system: &System,
    job: &JobBox,
    user: Option<&str>,
    process: &Process,
    stdio: [OwnedHandle; 3],
) -> Result<(Handle, u32), Refusal> {
    if job.stopping() {
        return Err(Refusal::NotRunning(job.id()?.try_clone()?, "exec"));
    }
    let launcher = system
        .create_process(&hcs::launcher_parameters(user).to_string(), PATIENCE)
        .map_err(|f| hcs_os("starting Hex's launcher in a container", f))?;
    let opened = sys::open_process(launcher.pid, sys::LAUNCHER_ACCESS)
        .map_err(|e| os("opening Hex's launcher in a container", e))?;
    let request = (|| {
        let dup = |h: std::os::windows::io::BorrowedHandle<'_>, access| {
            sys::duplicate_into(&opened, h, access)
                .map_err(|e| os("handing the launcher its box's job and streams", e))
        };
        Ok::<_, Refusal>(Launch {
            job: dup(job.job()?, Some(sys::JOB_ACCESS))?,
            stdio: [
                dup(stdio[0].as_handle(), None)?,
                dup(stdio[1].as_handle(), None)?,
                dup(stdio[2].as_handle(), None)?,
            ],
            command_line: process.command_line.clone(),
            env: process.env.clone(),
            cwd: process.cwd.clone(),
        })
    })();
    let request = match request {
        Ok(r) => r,
        Err(e) => {
            // The launcher waits on its stdin; ended, it holds nothing (CLAUDE.md §3: refused, not left).
            let _ = sys::terminate_process(&opened, UNANSWERED);
            return Err(e);
        }
    };
    let mut stdin = std::fs::File::from(launcher.stdin);
    let reply = hcs::encode(&request)
        .and_then(|line| {
            stdin
                .write_all(&line)
                .map_err(|e| os("writing to Hex's launcher", e))
        })
        .and_then(|()| reply(launcher.stdout));
    let reply = match reply {
        Ok(r) => r,
        Err(e) => {
            let _ = sys::terminate_process(&opened, UNANSWERED);
            return Err(e);
        }
    };
    match reply {
        Launched::Started { pid } => {
            let member = sys::open_process(pid, sys::MEMBER_ACCESS)
                .map_err(|e| os("opening a program started in a container", e))?;
            // The launcher holds the member's handle until its stdin closes, so `pid` is the member; it
            // must be in the box's job, or the box would not bound it.
            if !sys::in_job(&member, job.job()?).map_err(|e| os("checking a program's job", e))? {
                let _ = sys::terminate_process(&opened, UNANSWERED);
                return Err(Refusal::Broken(format!(
                    "the program Hex's launcher started (pid {pid}) isn't in its box's job"
                )));
            }
            // Closing stdin lets the launcher end; the member runs on in the box's job.
            drop(stdin);
            Ok((member, pid))
        }
        Launched::Full => Err(job.refused()?),
        Launched::Failed { op, code } => Err(Refusal::Os {
            op: "starting a program in a container",
            why: format!(
                "the launcher's {op} failed: {}",
                std::io::Error::from_raw_os_error(i32::try_from(code).unwrap_or(-1))
            ),
        }),
    }
}

/// Reads the launcher's one reply line from `stdout`, at most [`MAX_WIRE`] bytes, waiting at most
/// [`PATIENCE`] (on a thread of its own, which ends when the pipe does).
fn reply(stdout: OwnedHandle) -> Result<Launched, Refusal> {
    let (tx, rx) = mpsc::sync_channel(1);
    std::thread::Builder::new()
        .name("hex-oci-launch".to_owned())
        .spawn(move || {
            let mut line = Vec::new();
            let read = BufReader::new(std::fs::File::from(stdout))
                .take(
                    u64::try_from(MAX_WIRE)
                        .unwrap_or(u64::MAX)
                        .saturating_add(1),
                )
                .read_until(b'\n', &mut line);
            let _ = tx.send(read.map(|_| line));
        })
        .map_err(|e| os("starting the launcher's reply reader", e))?;
    let line = rx
        .recv_timeout(PATIENCE)
        .map_err(|_| Refusal::Os {
            op: "starting a program in a container",
            why: format!("Hex's launcher gave no reply in {PATIENCE:?}"),
        })?
        .map_err(|e| os("reading Hex's launcher's reply", e))?;
    if line.is_empty() {
        return Err(Refusal::Os {
            op: "starting a program in a container",
            why: "Hex's launcher ended without replying".to_owned(),
        });
    }
    hcs::decode(&line)
}

/// The launcher, run by HCS inside a container (`hex oci-launch`): reads the node's request, starts the
/// member suspended in the box's job with only its three standard handles inherited, resumes it, replies,
/// and holds the member's handle until its stdin closes. Returns its exit code: 0 when it replied, 2 when
/// the request was unusable (and the node, seeing no reply, refuses the exec).
pub fn serve() -> u8 {
    let mut input = BufReader::new(std::io::stdin().lock());
    let mut line = Vec::new();
    let limit = u64::try_from(MAX_WIRE)
        .unwrap_or(u64::MAX)
        .saturating_add(1);
    if (&mut input)
        .take(limit)
        .read_until(b'\n', &mut line)
        .is_err()
    {
        return 2;
    }
    let Ok(request) = hcs::decode::<Launch>(&line) else {
        return 2;
    };
    if hcs::check_launch(&request).is_err() {
        return 2;
    }
    let (reply, started) = launch(&request);
    let mut out = std::io::stdout().lock();
    let sent = hcs::encode(&reply)
        .ok()
        .is_some_and(|l| out.write_all(&l).and_then(|()| out.flush()).is_ok());
    if !sent {
        return 2;
    }
    // Hold the member's handle until the node closes stdin (EOF, or a broken pipe if the node ended).
    let mut sink = [0u8; 64];
    while matches!(input.read(&mut sink), Ok(n) if n > 0) {}
    drop(started);
    0
}

/// Starts `request`'s member; returns the reply and the member's handles (held until the node is done).
fn launch(request: &Launch) -> (Launched, Option<crate::windows::sys::Started>) {
    let failed = |op: &str, e: &std::io::Error| Launched::Failed {
        op: op.to_owned(),
        code: e
            .raw_os_error()
            .and_then(|c| u32::try_from(c).ok())
            .unwrap_or(0),
    };
    let handles = sys::adopt(request.job, request.stdio);
    let (job, stdio) = match handles {
        Ok(h) => h,
        Err(e) => return (failed("taking the node's handles", &e), None),
    };
    let mut spawn = Spawn {
        command_line: sys::wide(&request.command_line),
        environment: hex_core::job::environment(&request.env),
        cwd: request.cwd.as_deref().map(sys::wide),
        stdio,
    };
    let started = match crate::windows::sys::create(&mut spawn, &[job.as_raw_handle() as HANDLE]) {
        Ok(s) => s,
        Err(e) if crate::windows::sys::is_process_limit(&e) => return (Launched::Full, None),
        Err(e) => return (failed("CreateProcessW", &e), None),
    };
    if let Err(e) = crate::windows::sys::resume(&started.thread) {
        // A member that can't be resumed is ended, never left suspended in the box's job.
        let _ = sys::terminate_process(&started.process, UNANSWERED);
        return (failed("ResumeThread", &e), None);
    }
    (Launched::Started { pid: started.pid }, Some(started))
}
