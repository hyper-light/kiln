//! The `process` class on Linux: host processes started straight into their box's cgroup, so a
//! program and everything it forks are bound from their first instruction (CP-T7.1). They are cloned
//! into the cgroup (`clone3` with `CLONE_INTO_CGROUP`) where the kernel can, and otherwise forked and
//! joined to it before they execute.

use std::ffi::CString;
use std::os::fd::{AsFd, BorrowedFd, OwnedFd};
use std::process::{Command, Stdio as StdStdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use hex_api::{ExecLimit, Program, Refusal, Stdio};
use rustix::fs::{Mode, OFlags};

use super::cgroup::BoxCgroup;
use super::proc::PidfdProc;
use super::sys;

/// The node's own RLIMIT_STACK soft limit, read once (0 until then; `u64::MAX` for none) and again only when
/// the kernel answers E2BIG past the bound Hex held a program to (learned).
static NODE_STACK: AtomicU64 = AtomicU64::new(0);
/// How many times the node's stack limit was read, so a test can tell no exec reads it.
static STACK_READS: AtomicU64 = AtomicU64::new(0);

fn read_stack() -> u64 {
    STACK_READS.fetch_add(1, Ordering::Relaxed);
    rustix::process::getrlimit(rustix::process::Resource::Stack)
        .current
        .unwrap_or(u64::MAX)
        .max(1)
}

/// The node's RLIMIT_STACK soft limit: read on first use, then kept.
pub fn node_stack() -> u64 {
    match NODE_STACK.load(Ordering::Relaxed) {
        0 => {
            let stack = read_stack();
            NODE_STACK.store(stack, Ordering::Relaxed);
            stack
        }
        stack => stack,
    }
}

/// Reads the node's stack limit again, after the kernel refused what its last reading admitted.
pub fn relearn_stack() {
    NODE_STACK.store(read_stack(), Ordering::Relaxed);
}

/// How many times the node's stack limit has been read.
pub fn stack_reads() -> u64 {
    STACK_READS.load(Ordering::Relaxed)
}

/// The bound a box's programs are held to: [`exec_limit_for`] the node's own stack limit, read once.
pub fn exec_limit() -> ExecLimit {
    exec_limit_for(node_stack())
}

/// Holds a spec's process (its arguments and `KEY=VALUE` environment, as execve takes them) to the bound
/// of a box whose stack limit is `stack`.
pub fn fits_process(args: &[String], env: &[String], stack: u64) -> Result<(), Refusal> {
    let program = Program {
        args: args.to_vec(),
        env: env
            .iter()
            .map(|e| {
                let (k, v) = e.split_once('=').unwrap_or((e.as_str(), ""));
                (k.to_owned(), v.to_owned())
            })
            .collect(),
        ..Program::default()
    };
    program.fits(&exec_limit_for(stack))
}

/// The bound Linux's execve puts on a program's arguments and environment in a process whose RLIMIT_STACK
/// soft limit is `stack`, kind (a) (docs/research/runtime/exec-size-bounds.md): the strings may take a
/// quarter of the stack, capped at three quarters of `_STK_LIM` and never under `ARG_MAX`
/// (`fs/exec.c:410-416`, v6.19), and any one string `MAX_ARG_STRLEN`, 32 pages
/// (`include/uapi/linux/binfmts.h:15`). glibc's `sysconf(_SC_ARG_MAX)` skips the kernel's cap, so the
/// kernel's own formula is used. No system call: the page size is the auxiliary vector's.
pub fn exec_limit_for(stack: u64) -> ExecLimit {
    // `_STK_LIM`, include/uapi/linux/resource.h:66, and `ARG_MAX`, include/uapi/linux/limits.h:8 (v6.19).
    const STK_LIM: u64 = 8 << 20;
    const ARG_MAX: u64 = 131_072;
    // `MAX_ARG_STRLEN` is this many pages, include/uapi/linux/binfmts.h:15.
    const MAX_ARG_STRLEN_PAGES: usize = 32;
    let quarter = stack.checked_div(4).unwrap_or(0);
    let cap = STK_LIM.checked_div(4).unwrap_or(0).saturating_mul(3);
    let block = cap.min(quarter).max(ARG_MAX);
    ExecLimit {
        block: usize::try_from(block).unwrap_or(usize::MAX),
        counts: hex_api::BlockCount::Execve,
        string: Some(rustix::param::page_size().saturating_mul(MAX_ARG_STRLEN_PAGES)),
        unit: hex_api::StringUnit::Bytes,
        source: "Linux's execve (a quarter of the box's RLIMIT_STACK, fs/exec.c)",
    }
}

/// Starts `program` in the box `cg` with `stdio`, as the node agent's own user.
pub fn exec(cg: &BoxCgroup, program: &Program, stdio: Stdio) -> Result<PidfdProc, Refusal> {
    program.check()?;
    program.fits(&exec_limit())?;
    if program.user.is_some() {
        return Err(Refusal::Invalid {
            what: "program",
            why: "a process box runs its programs as the node agent's user; a user is the oci class's".to_owned(),
        });
    }
    let first = program.args.first().ok_or(Refusal::Invalid {
        what: "program",
        why: "no arguments".to_owned(),
    })?;
    let failed = |e: std::io::Error| {
        if e.raw_os_error() == Some(rustix::io::Errno::TOOBIG.raw_os_error()) {
            // The kernel refused what the bound admitted: its stack limit changed. Learn it again.
            relearn_stack();
            return Refusal::Invalid {
                what: "program",
                why: format!(
                    "{first}: the kernel refused its arguments and environment (E2BIG); the node's stack limit is read again"
                ),
            };
        }
        Refusal::Os {
            op: "starting a program in a box",
            why: format!("{first}: {e}"),
        }
    };
    let stdio = [
        stdio
            .stdin
            .map_or_else(|| null(OFlags::RDONLY), |p| Ok(OwnedFd::from(p)))?,
        stdio
            .stdout
            .map_or_else(|| null(OFlags::WRONLY), |p| Ok(OwnedFd::from(p)))?,
        stdio
            .stderr
            .map_or_else(|| null(OFlags::WRONLY), |p| Ok(OwnedFd::from(p)))?,
    ];
    let spawn = sys::Spawn::new(
        paths(program)?,
        program
            .args
            .iter()
            .map(|a| c_string(a))
            .collect::<Result<_, _>>()?,
        program
            .env
            .iter()
            .map(|(k, v)| c_string(&format!("{k}={v}")))
            .collect::<Result<_, _>>()?,
        program.cwd.as_deref().map(c_string).transpose()?,
        stdio,
        super::default_nofile(),
    )
    .map_err(failed)?;
    let dir = cg.open_dir()?;
    match sys::clone_into_cgroup(dir.as_fd(), spawn).map_err(failed)? {
        sys::Cloned::Started { pid, pidfd } => Ok(PidfdProc::of_clone(pid, pidfd)),
        sys::Cloned::Unsupported(spawn) => exec_forked(cg, program, spawn.into_stdio()),
    }
}

/// A managed VM controller created in its native envelope but held before foreign execution.
/// The caller transfers this receipt into its preallocated Supervisor slot before reading or
/// publishing its identity. Dropping the gate cancels execution; the SAME slot still reaps it.
pub struct HeldExec(sys::HeldChild);

impl HeldExec {
    pub fn pidfd(&self) -> BorrowedFd<'_> {
        self.0.pidfd()
    }

    /// The kernel's native return width is retained until the native owner has this receipt.
    /// Identity conversion/observation must not discard an already-created child on failure.
    pub fn into_parts(self) -> (libc::c_long, OwnedFd, sys::HeldGate) {
        self.0.into_parts()
    }
}

/// One admitted report, consumed by the final native held-exec boundary. No raw parent
/// descriptor is encoded; that boundary duplicates/owns the descriptor used by foreign exec.
pub struct ManagedReport {
    pub scope: [u8; 16],
    pub writer: std::io::PipeWriter,
}

/// What a held VM's launch carries past its program and stdio: its admitted final GPU report, and the locked-memory
/// limit its VMM starts under where it isn't the node's own (a VM given a device whole, whose VMM pins its RAM for
/// DMA: hex-runtime's firecracker vfio).
#[derive(Default)]
pub struct HeldLaunch {
    pub report: Option<ManagedReport>,
    pub memlock: Option<rustix::process::Rlimit>,
}

/// Start an explicitly configured VM binary, without executing it until its native identity and
/// canonical record have been published. `gate_budget` is the caller's remaining finite create
/// budget, never a process-completion timer. Ordinary `exec` retains its existing vfork path.
pub fn exec_held(
    cg: &BoxCgroup,
    program: &Program,
    stdio: Stdio,
    launch: HeldLaunch,
    gate_budget: Duration,
) -> Result<HeldExec, Refusal> {
    let HeldLaunch { report, memlock } = launch;
    program.check()?;
    program.fits(&exec_limit())?;
    if program.user.is_some() || !program.args.first().is_some_and(|arg| arg.starts_with('/')) {
        return Err(held_invalid(
            "a managed VM requires an absolute configured binary and the node's own user",
        ));
    }
    let report = report
        .map(|report| {
            if report.scope == [0; 16] {
                return Err(held_invalid("GPU report scope is empty"));
            }
            let descriptor =
                rustix::io::fcntl_dupfd_cloexec(&report.writer, 3).map_err(|error| {
                    Refusal::os("duplicating the final GPU report descriptor", error)
                })?;
            drop(report.writer);
            let words = shards_api::physical::report_arguments(report.scope, &descriptor)
                .map_err(|_| held_invalid("final GPU report arguments could not be rendered"))?;
            check_report_words(program, &words, &exec_limit())?;
            Ok((descriptor, words))
        })
        .transpose()?;
    let mut paths = Vec::new();
    paths.try_reserve_exact(1).map_err(|_| held_storage(1))?;
    let first = program
        .args
        .first()
        .ok_or_else(|| held_invalid("managed VM has no binary"))?;
    paths.push(held_string(&[first])?);
    let mut argv = Vec::new();
    let count = program
        .args
        .len()
        .checked_add(report.as_ref().map_or(0, |(_, words)| words.len()))
        .ok_or_else(|| held_storage(usize::MAX))?;
    argv.try_reserve_exact(count)
        .map_err(|_| held_storage(count))?;
    for arg in &program.args {
        argv.push(held_string(&[arg])?);
    }
    if let Some((_, words)) = &report {
        for word in words {
            argv.push(held_string(&[word.to_str().ok_or_else(|| {
                held_invalid("GPU report argument is not UTF-8")
            })?])?);
        }
    }
    let mut envp = Vec::new();
    envp.try_reserve_exact(program.env.len())
        .map_err(|_| held_storage(program.env.len()))?;
    for (key, value) in &program.env {
        envp.push(held_string(&[key, "=", value])?);
    }
    let cwd = program
        .cwd
        .as_deref()
        .map(|cwd| held_string(&[cwd]))
        .transpose()?;
    let stdio = [
        stdio
            .stdin
            .map_or_else(|| null(OFlags::RDONLY), |pipe| Ok(OwnedFd::from(pipe)))?,
        stdio
            .stdout
            .map_or_else(|| null(OFlags::WRONLY), |pipe| Ok(OwnedFd::from(pipe)))?,
        stdio
            .stderr
            .map_or_else(|| null(OFlags::WRONLY), |pipe| Ok(OwnedFd::from(pipe)))?,
    ];
    let spawn = sys::Spawn::new(paths, argv, envp, cwd, stdio, super::default_nofile())
        .map_err(|error| Refusal::os("preparing held VM descriptors", error))?
        .with_memlock(memlock);
    let dir = cg.open_dir()?;
    let procs = cg.procs_for_joining()?;
    sys::spawn_held(
        dir.as_fd(),
        procs.as_fd(),
        spawn,
        report.map(|(descriptor, _)| descriptor),
        gate_budget,
    )
    .map(HeldExec)
    .map_err(|error| Refusal::os("creating a held VM in its native envelope", error))
}

fn check_report_words(
    program: &Program,
    words: &[std::ffi::OsString; 4],
    limit: &ExecLimit,
) -> Result<(), Refusal> {
    let mut total = program.exec_size();
    for word in words {
        let word = word
            .to_str()
            .ok_or_else(|| held_invalid("GPU report argument is not UTF-8"))?;
        let bytes = word
            .len()
            .checked_add(1)
            .ok_or_else(|| held_storage(usize::MAX))?;
        if limit.string.is_some_and(|most| bytes > most) {
            return Err(held_invalid(
                "GPU report argument exceeds the native string bound",
            ));
        }
        total = total
            .checked_add(bytes)
            .and_then(|total| total.checked_add(size_of::<usize>()))
            .ok_or_else(|| held_storage(usize::MAX))?;
    }
    if total > limit.block {
        return Err(held_invalid(
            "GPU report suffix exceeds the native exec block",
        ));
    }
    Ok(())
}

fn held_storage(bound: usize) -> Refusal {
    Refusal::Exhausted {
        what: "held VM launch storage",
        bound,
    }
}

fn held_invalid(message: &'static str) -> Refusal {
    let mut why = String::new();
    if why.try_reserve_exact(message.len()).is_err() {
        return held_storage(message.len());
    }
    why.push_str(message);
    Refusal::Invalid {
        what: "held VM program",
        why,
    }
}

/// Reserve the complete NUL-terminated string before constructing CString. Rust 1.99's
/// from_vec_with_nul calls into_boxed_slice: equality of capacity/length prevents a hidden shrink
/// allocation there (library/alloc/src/ffi/c_str.rs and vec/mod.rs, pinned in the evidence note).
fn held_string(parts: &[&str]) -> Result<CString, Refusal> {
    let mut room = 1usize;
    for part in parts {
        if part.contains('\0') {
            return Err(held_invalid("a managed VM argument contains NUL"));
        }
        room = room
            .checked_add(part.len())
            .ok_or_else(|| held_storage(usize::MAX))?;
    }
    let mut bytes = Vec::new();
    bytes
        .try_reserve_exact(room)
        .map_err(|_| held_storage(room))?;
    for part in parts {
        bytes.extend_from_slice(part.as_bytes());
    }
    bytes.push(0);
    if bytes.capacity() != bytes.len() {
        return Err(held_storage(room));
    }
    CString::from_vec_with_nul(bytes)
        .map_err(|_| held_invalid("a managed VM argument is not a single C string"))
}

/// Where the kernel or a seccomp profile refuses a clone into a cgroup: forked by std's `Command`,
/// which then joins the cgroup by migration in the child before it executes.
fn exec_forked(
    cg: &BoxCgroup,
    program: &Program,
    stdio: [OwnedFd; 3],
) -> Result<PidfdProc, Refusal> {
    let (first, rest) = program.args.split_first().ok_or(Refusal::Invalid {
        what: "program",
        why: "no arguments".to_owned(),
    })?;
    let [stdin, stdout, stderr] = stdio;
    let mut cmd = Command::new(first);
    cmd.args(rest)
        .env_clear()
        .envs(program.env.iter().map(|(k, v)| (k, v)))
        .stdin(StdStdio::from(stdin))
        .stdout(StdStdio::from(stdout))
        .stderr(StdStdio::from(stderr));
    if let Some(cwd) = &program.cwd {
        cmd.current_dir(cwd);
    }
    // A box the node starts gets the default limit, not the node's own (HEX-INST-6).
    sys::join_cgroup_on_exec(
        &mut cmd,
        Arc::new(cg.procs_for_joining()?),
        Some(super::default_nofile()),
    );
    let child = cmd.spawn().map_err(|e| Refusal::Os {
        op: "starting a program in a box",
        why: format!("{first}: {e}"),
    })?;
    // The child stays unwaited (dropping a std Child neither waits nor kills), so its pid is ours
    // until the pidfd reaps it.
    PidfdProc::of_child(child.id())
}

/// The paths a program is executed from, in order, as execvp(3) tries them: its name when it holds a
/// `/`, else that name in each folder of its own `PATH`, or of glibc's default `/bin:/usr/bin` when it
/// sets none (an empty entry is the working folder). The same as std's `Command`, which hands execvp
/// the child's environment.
fn paths(program: &Program) -> Result<Vec<CString>, Refusal> {
    let first = program.args.first().ok_or(Refusal::Invalid {
        what: "program",
        why: "no arguments".to_owned(),
    })?;
    if first.contains('/') {
        return Ok(vec![c_string(first)?]);
    }
    let path = program
        .env
        .iter()
        .find(|(k, _)| k == "PATH")
        .map(|(_, v)| v.as_str())
        .unwrap_or("/bin:/usr/bin");
    path.split(':')
        .map(|dir| {
            c_string(&format!(
                "{}/{first}",
                if dir.is_empty() { "." } else { dir }
            ))
        })
        .collect()
}

fn c_string(s: &str) -> Result<CString, Refusal> {
    CString::new(s).map_err(|_| Refusal::Invalid {
        what: "program",
        why: format!("{s:?} holds a NUL byte"),
    })
}

/// The null device, for a stream the caller gave none.
fn null(flags: OFlags) -> Result<OwnedFd, Refusal> {
    rustix::fs::open("/dev/null", flags | OFlags::CLOEXEC, Mode::empty())
        .map_err(|e| Refusal::os("opening /dev/null", e))
}

/// Makes `cmd`'s child start in the box `cg`: for callers that start processes themselves (kiln's
/// runner keeps its own stdio and process-group handling, CP-T7.1's first consumer, and `hex box run`).
/// The program keeps its caller's descriptor limit: such a caller wraps a program for itself, and isn't
/// the node, whose raised limit the default for the node's own boxes keeps them from (HEX-INST-6).
pub fn join_on_exec(cg: &BoxCgroup, cmd: &mut Command) -> Result<(), Refusal> {
    sys::join_cgroup_on_exec(cmd, Arc::new(cg.procs_for_joining()?), None);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The fixed report suffix consumes native exec space before clone; metadata-only control.
    #[test]
    fn managed_report_suffix_uses_the_existing_exec_block_and_string_bounds() {
        let program = Program::of(&["/fixture/shards", "run", "--kernel", "/fixture/kernel"]);
        let words = [
            "--gpu-report",
            "7",
            "--gpu-report-scope",
            "00112233445566778899aabbccddeeff",
        ]
        .map(std::ffi::OsString::from);
        let extra: usize = words
            .iter()
            .map(|word| word.len() + 1 + size_of::<usize>())
            .sum();
        let limit = ExecLimit {
            block: program.exec_size() + extra,
            counts: hex_api::BlockCount::Execve,
            string: None,
            unit: hex_api::StringUnit::Bytes,
            source: "synthetic exact exec boundary",
        };
        check_report_words(&program, &words, &limit).unwrap();
        assert!(
            check_report_words(
                &program,
                &words,
                &ExecLimit {
                    block: limit.block - 1,
                    ..limit
                }
            )
            .is_err()
        );
        assert!(
            check_report_words(
                &program,
                &words,
                &ExecLimit {
                    string: Some(1),
                    ..limit
                }
            )
            .is_err()
        );
    }

    /// AC-T7.4.1: prepared VM arguments preserve exact bytes and reject embedded terminators.
    #[test]
    fn held_vm_arguments_are_exact_and_nul_checked_before_creation() {
        assert_eq!(
            held_string(&["KEY", "=", "a b"]).unwrap().as_bytes(),
            b"KEY=a b"
        );
        assert_eq!(held_string(&[""]).unwrap().as_bytes_with_nul(), b"\0");
        assert!(held_string(&["bad\0argument"]).is_err());
        assert_eq!(
            held_string(&["/absolute/", "vm"]).unwrap().as_bytes(),
            b"/absolute/vm"
        );
    }

    /// HEX-RT-14: the bound a box's exec is held to comes from the box's own RLIMIT_STACK, and reading it
    /// takes no system call: the node reads its own limit once. Do: the bound for a box whose spec sets a
    /// 1 MiB stack, a hundred times, after the node's bound has been read. Expect a quarter of it (256
    /// KiB, above Linux's 128 KiB floor and under the node's default), and no getrlimit among the hundred.
    #[test]
    fn a_box_with_a_smaller_stack_gets_the_smaller_bound_and_no_read_per_exec() {
        let node = exec_limit();
        let reads = stack_reads();
        for _ in 0..100 {
            let bound = exec_limit_for(1 << 20);
            assert_eq!(bound.block, 256 << 10);
            let _ = exec_limit();
        }
        assert_eq!(stack_reads(), reads, "a getrlimit per exec");
        assert!(node.block >= 256 << 10, "{node:?}");
    }
}
