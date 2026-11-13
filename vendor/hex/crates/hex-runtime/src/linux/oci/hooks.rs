//! The OCI lifecycle hooks (runtime-spec v1.3.0 config.md:548-661, docs/research/runtime/oci-hooks.md):
//! running a list of them with the container's state on each one's stdin, each within its timeout, and
//! the frames that carry a state between the runtime and the container's init, which runs the hooks the
//! spec puts in the container's namespaces (createContainer, startContainer).

use std::cell::Cell;
use std::io::Write as _;
use std::os::fd::{AsFd, BorrowedFd, OwnedFd};
use std::os::unix::process::CommandExt as _;
use std::process::Command;
use std::time::{Duration, Instant};

use rustix::event::{PollFd, PollFlags};
use rustix::fs::OFlags;

use crate::linux::oci::OciError;
use crate::linux::oci::run::MAX_ID_BYTES;
use crate::linux::oci::spec::{Hook, Hooks, MAX_CONFIG_BYTES};
use crate::linux::oci::state::State;

/// The most a state handed to the init may hold: its annotations, which take no more than the config's text of
/// them (serde_json escapes nothing a config could write shorter, RFC 8259 §7), so at most [`MAX_CONFIG_BYTES`];
/// and its other fields at their widest: the shape of a state with those strings taken out
/// ([`STATE_SHAPE`]), a container id (at most [`MAX_ID_BYTES`], none of whose characters is escaped), and its
/// bundle and cgroup paths, each at most `PATH_MAX` less its NUL, every byte at worst a six-byte `\u00XX`.
pub const MAX_STATE_BYTES: usize =
    MAX_CONFIG_BYTES + STATE_SHAPE.len() + (MAX_ID_BYTES + 2) + 2 * ((PATH_MAX - 1) * 6 + 2);

/// A state's JSON at its widest with its id, its paths and its annotations taken out: every key, the longest
/// status, and each number its widest (`i32::MIN`, `u64::MAX`), in the order serde writes them ([`State`]).
const STATE_SHAPE: &str = r#"{"ociVersion":"1.3.0","id":,"status":"creating","pid":-2147483648,"bundle":,"annotations":,"cgroup":,"startTime":18446744073709551615,"poststart":false,"pidNamespace":false,"startContainer":18446744073709551615}"#;

/// `PATH_MAX`, a path's most bytes with its NUL (include/uapi/linux/limits.h).
const PATH_MAX: usize = 4096;

/// The most a startContainer failure written on the exec FIFO holds, its tag byte included: `PIPE_BUF`
/// (pipe(7)), so the init's one write lands whole and `start` reads it whole without taking it.
pub const MAX_START_FAILURE: usize = 4096;

/// The exec FIFO's byte for a start taken; a startContainer failure is [`START_FAILED`] and why.
pub const START_TAKEN: u8 = b'0';
pub const START_FAILED: u8 = b'h';

/// The init's exit status when the start that opened its exec FIFO ended before the init could hand it
/// the byte: its write found the FIFO's reader gone (EPIPE; the init, a `hex` process, has SIGPIPE ignored,
/// as Rust's runtime leaves every program it starts). With startContainer hooks that window lasts as long
/// as they run, so a node agent stopped then leaves an init that ends with this status, never by a bare
/// SIGPIPE. Distinct from the init's 127 for a set-up that failed.
pub const START_ABANDONED: u8 = 122;

/// The file in a container's state directory that says a start of a container with startContainer hooks
/// has begun: made before the start waits on the exec FIFO, removed when the start returns. Found beside
/// a FIFO still named, it says the start never took the byte: the node agent stopped during the start
/// ([`crate::linux::oci::state::start_interrupted`]), so recovery removes the box by that reason.
pub const START_BEGUN: &str = "start-begun";

thread_local! {
    /// This thread's wakes while it waits on a hook so far ([`wakes_on_this_thread`]).
    static WAKES: Cell<u64> = const { Cell::new(0) };
}

/// How many times this thread has woken while waiting on a hook: once each time it looks again at whether the hook
/// has taken its state or ended. Tests read it.
pub fn wakes_on_this_thread() -> u64 {
    WAKES.with(Cell::get)
}

/// One more wake on this thread.
fn woke() {
    WAKES.with(|n| n.set(n.get().saturating_add(1)));
}

impl Hooks {
    /// Whether the init stops before its root switch for the runtime: it runs prestart and createRuntime
    /// there (config.md:607) and hands the init the state its createContainer and startContainer hooks
    /// are given.
    pub fn sync_init(&self) -> bool {
        !(self.prestart.is_empty()
            && self.create_runtime.is_empty()
            && self.create_container.is_empty()
            && self.start_container.is_empty())
    }
}

/// The seconds `hooks` may take together, the sum of their timeouts; `None` where one names none, which is waited
/// for as long as it runs. Refused, naming `kind`, where a timeout isn't above zero (config.md:559-560) or the sum
/// overflows.
pub fn budget(kind: &str, hooks: &[Hook]) -> Result<Option<u64>, OciError> {
    hooks.iter().try_fold(Some(0_u64), |sum, hook| {
        let Some(limit) = timeout(kind, hook)? else {
            return Ok(None);
        };
        match sum {
            None => Ok(None),
            Some(sum) => sum
                .checked_add(limit.as_secs())
                .map(Some)
                .ok_or_else(|| OciError::Config(format!("the {kind} hooks' timeouts overflow"))),
        }
    })
}

/// How long `hook` may run: the timeout it names, or `None` where it names none. The spec gives no default
/// (config.md:559-560), and runc waits for such a hook as long as it runs (`Command.Run`, runc 1.6.0-rc.1
/// libcontainer/configs/config.go:620-635), as crun does (`run_process_with_stdin_timeout_envp`, crun 1.30.1
/// src/libcrun/utils.c:1902, a timeout of 0 waits): so does Hex, and its caller's own patience bounds the whole.
fn timeout(kind: &str, hook: &Hook) -> Result<Option<Duration>, OciError> {
    match hook.timeout {
        None => Ok(None),
        Some(t) => u64::try_from(t)
            .ok()
            .filter(|t| *t > 0)
            .map(|t| Some(Duration::from_secs(t)))
            .ok_or_else(|| {
                OciError::Config(format!(
                    "a {kind} hook's timeout is {t}: it must be above zero"
                ))
            }),
    }
}

/// Runs each hook in order with `state` as JSON on its stdin (config.md:592); a hook that can't start,
/// exits non-zero or outlives its timeout fails, as `op` (the list's name).
pub fn run(op: &'static str, hooks: &[Hook], state: &State) -> Result<(), OciError> {
    if hooks.is_empty() {
        return Ok(());
    }
    let json = serde_json::to_vec(state)
        .map_err(|e| OciError::Broken(format!("encoding state for a hook: {e}")))?;
    for hook in hooks {
        run_one(op, hook, &json)?;
    }
    Ok(())
}

fn run_one(op: &'static str, hook: &Hook, state_json: &[u8]) -> Result<(), OciError> {
    let mut cmd = Command::new(&hook.path);
    if let Some(argv0) = hook.args.first() {
        cmd.arg0(argv0);
        cmd.args(hook.args.get(1..).unwrap_or_default());
    }
    cmd.env_clear();
    for entry in &hook.env {
        if let Some((k, v)) = entry.split_once('=') {
            cmd.env(k, v);
        }
    }
    cmd.stdin(std::process::Stdio::piped());
    crate::linux::sys::sweep_on_exec(&mut cmd);
    let failed = |why: String| OciError::Os { op, why };
    let mut child = cmd
        .spawn()
        .map_err(|e| failed(format!("{:?} didn't start: {e}", hook.path)))?;
    let limit = timeout(op, hook)?;
    // Its pidfd turns readable once it has ended (pidfd_open(2)); its pid stays its own until it's reaped here.
    let pidfd = match i32::try_from(child.id())
        .ok()
        .and_then(rustix::process::Pid::from_raw)
        .ok_or(rustix::io::Errno::SRCH)
        .and_then(|pid| rustix::process::pidfd_open(pid, rustix::process::PidfdFlags::empty()))
    {
        Ok(pidfd) => pidfd,
        Err(e) => {
            let _ = child.kill();
            let _ = child.wait();
            return Err(OciError::os("pidfd_open of a hook", e));
        }
    };
    // The state goes in as the hook takes it, never blocking: a hook that doesn't read its stdin can't hold
    // the runtime past its timeout.
    let mut stdin = child.stdin.take();
    if let Some(pipe) = &stdin {
        rustix::fs::fcntl_setfl(pipe, OFlags::NONBLOCK)
            .map_err(|e| OciError::os("making a hook's stdin non-blocking", e))?;
    }
    let mut left = state_json;
    // None where the hook names no timeout, or one past what an Instant holds.
    let deadline = limit.and_then(|limit| Instant::now().checked_add(limit));
    loop {
        if let Some(pipe) = stdin.as_mut() {
            match pipe.write(left) {
                Ok(n) => left = left.get(n..).unwrap_or_default(),
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {}
                // A hook that closed its stdin has the state it wanted.
                Err(e) if e.kind() == std::io::ErrorKind::BrokenPipe => left = &[],
                Err(e) => return Err(OciError::os("writing a hook's state", e)),
            }
            if left.is_empty() {
                stdin = None;
            }
        }
        match child
            .try_wait()
            .map_err(|e| OciError::os("waiting for a hook", e))?
        {
            Some(status) if status.success() => return Ok(()),
            Some(status) => return Err(failed(format!("{:?} exited {status}", hook.path))),
            None => {
                let left = deadline.map(|d| d.saturating_duration_since(Instant::now()));
                if left.is_some_and(|l| l.is_zero()) {
                    let _ = child.kill();
                    let _ = child.wait();
                    return Err(failed(format!(
                        "{:?} outlived its {:?} timeout",
                        hook.path,
                        limit.unwrap_or_default()
                    )));
                }
                wait_on(&pidfd, stdin.as_ref(), left)?;
            }
        }
    }
}

/// Waits until the hook behind `pidfd` has ended, its stdin `stdin` (while the state isn't all in) takes more, or
/// `left` passes (`None`: no deadline).
fn wait_on(
    pidfd: &OwnedFd,
    stdin: Option<&std::process::ChildStdin>,
    left: Option<Duration>,
) -> Result<(), OciError> {
    let ts = left.map(|left| rustix::event::Timespec {
        tv_sec: i64::try_from(left.as_secs()).unwrap_or(i64::MAX),
        tv_nsec: i64::from(left.subsec_nanos()),
    });
    let mut fds = vec![PollFd::new(pidfd, PollFlags::IN)];
    if let Some(pipe) = stdin {
        fds.push(PollFd::new(pipe, PollFlags::OUT));
    }
    match rustix::event::poll(&mut fds, ts.as_ref()) {
        Ok(_) | Err(rustix::io::Errno::INTR) => {
            woke();
            Ok(())
        }
        Err(e) => Err(OciError::os("waiting for a hook", e)),
    }
}

/// Writes `state` to the init as a frame: its length (4 bytes, little-endian), then its JSON.
pub fn send_state(to: &mut impl std::io::Write, state: &State) -> Result<(), OciError> {
    let json = serde_json::to_vec(state)
        .map_err(|e| OciError::Broken(format!("encoding state for the init's hooks: {e}")))?;
    let len = u32::try_from(json.len())
        .ok()
        .filter(|_| json.len() <= MAX_STATE_BYTES)
        .ok_or_else(|| {
            OciError::Config(format!(
                "the container's state is {} bytes, over the {MAX_STATE_BYTES} its hooks are given",
                json.len()
            ))
        })?;
    to.write_all(&len.to_le_bytes())
        .and_then(|()| to.write_all(&json))
        .map_err(|e| OciError::os("handing the init its hooks' state", e))
}

/// Reads the frame [`send_state`] wrote. An end before it says the runtime refused the create (a
/// createRuntime hook failed, say) and closed its end.
pub fn read_state(from: BorrowedFd<'_>) -> Result<State, OciError> {
    let mut len = [0_u8; 4];
    read_exact(from, &mut len)?;
    let len = usize::try_from(u32::from_le_bytes(len))
        .ok()
        .filter(|n| *n <= MAX_STATE_BYTES)
        .ok_or_else(|| OciError::Broken("the hooks' state is over its bound".to_owned()))?;
    let mut json = vec![0_u8; len];
    read_exact(from, &mut json)?;
    serde_json::from_slice(&json)
        .map_err(|e| OciError::Broken(format!("the hooks' state isn't one: {e}")))
}

fn read_exact(from: BorrowedFd<'_>, mut buf: &mut [u8]) -> Result<(), OciError> {
    while !buf.is_empty() {
        match rustix::io::read(from, &mut *buf) {
            Ok(0) => {
                return Err(OciError::Broken(
                    "the runtime ended the create before handing the hooks' state".to_owned(),
                ));
            }
            Ok(n) => buf = std::mem::take(&mut buf).get_mut(n..).unwrap_or_default(),
            Err(rustix::io::Errno::INTR) => {}
            Err(e) => return Err(OciError::os("reading the hooks' state", e)),
        }
    }
    Ok(())
}

/// What the init writes on the exec FIFO when a startContainer hook fails: [`START_FAILED`], then why,
/// cut to [`MAX_START_FAILURE`] at a character's edge.
pub fn start_failure(e: &OciError) -> Vec<u8> {
    let why = e.to_string();
    let mut end = why.len().min(MAX_START_FAILURE.saturating_sub(1));
    while !why.is_char_boundary(end) {
        end = end.saturating_sub(1);
    }
    let mut out = Vec::with_capacity(end.saturating_add(1));
    out.push(START_FAILED);
    out.extend_from_slice(why.get(..end).unwrap_or_default().as_bytes());
    out
}

/// Reads what the init wrote on the exec FIFO `fifo` without taking it (`tee(2)` into a pipe of its own),
/// so another start at once still finds the byte: `None` for a start taken, why for a startContainer
/// failure.
pub fn peek_start(fifo: BorrowedFd<'_>) -> Result<Option<String>, OciError> {
    let (r, w) = rustix::pipe::pipe_with(rustix::pipe::PipeFlags::CLOEXEC)
        .map_err(|e| OciError::os("pipe", e))?;
    let n = rustix::pipe::tee(
        fifo,
        w.as_fd(),
        MAX_START_FAILURE,
        rustix::pipe::SpliceFlags::NONBLOCK,
    )
    .map_err(|e| OciError::os("reading the exec fifo", e))?;
    let mut buf = vec![0_u8; n];
    read_exact(r.as_fd(), &mut buf)?;
    match buf.split_first() {
        Some((&START_TAKEN, _)) => Ok(None),
        Some((&START_FAILED, why)) => Ok(Some(String::from_utf8_lossy(why).into_owned())),
        _ => Err(OciError::Broken(format!(
            "the exec fifo holds {buf:?}, not a start's byte"
        ))),
    }
}

#[cfg(test)]
mod tests {
    //! The hooks' runner and frames (AC-T7.1.4, T-T7.1.4; CLAUDE.md §2, §3; config.md:556-560, :592).

    use super::*;
    use crate::linux::oci::spec::Spec;

    fn sh(script: &str, timeout: Option<i64>) -> Hook {
        Hook {
            path: "/bin/sh".to_owned(),
            args: vec!["sh".to_owned(), "-c".to_owned(), script.to_owned()],
            env: vec!["PATH=/usr/bin:/bin".to_owned()],
            timeout,
        }
    }

    /// Do: hand a state through a frame. Expect: the same state back.
    #[test]
    fn a_state_handed_through_a_frame_comes_back_whole() {
        let mut state = State::new("c1", "/bundle");
        state.pid = 42;
        state.annotations.insert("k".to_owned(), "v".to_owned());
        let (r, w) = rustix::pipe::pipe_with(rustix::pipe::PipeFlags::CLOEXEC).unwrap();
        let mut w = std::fs::File::from(w);
        send_state(&mut w, &state).unwrap();
        drop(w);
        assert_eq!(read_state(r.as_fd()).unwrap(), state);
    }

    /// Hostile input to the frame's reader. Do: a length over [`MAX_STATE_BYTES`], then a frame cut short.
    /// Expect: each refused by type, nothing allocated for the first.
    #[test]
    fn a_frame_over_its_bound_or_cut_short_is_refused() {
        let over = u32::try_from(MAX_STATE_BYTES + 1).unwrap().to_le_bytes();
        for wire in [over.to_vec(), [4, 0, 0, 0, b'{'].to_vec(), vec![1, 0]] {
            let (r, w) = rustix::pipe::pipe_with(rustix::pipe::PipeFlags::CLOEXEC).unwrap();
            rustix::io::write(&w, &wire).unwrap();
            drop(w);
            assert!(
                matches!(read_state(r.as_fd()), Err(OciError::Broken(_))),
                "{wire:?}"
            );
        }
    }

    /// AC-T7.1.4; Ada's 2026-11-06 rule (no picked constants): the state's bound is the config's, which holds its
    /// annotations, and the widest every other field may be, not twice the config's. Do: write the widest state there
    /// is with no annotations: its id the longest a container's may be, its bundle and cgroup paths `PATH_MAX`'s
    /// bytes less its NUL, each byte one JSON escapes to six (`\u0001`), its numbers their widest. Expect: the bound is
    /// the config's and that state's bytes, less the empty annotations' `{}`.
    #[test]
    fn the_states_bound_is_the_configs_and_its_widest_other_fields() {
        let path = "\u{1}".repeat(usize::try_from(libc::PATH_MAX).unwrap() - 1);
        let mut widest = State::new(&"a".repeat(crate::linux::oci::run::MAX_ID_BYTES), &path);
        widest.status = crate::linux::oci::state::Status::Creating;
        widest.pid = i32::MIN;
        widest.cgroup = Some(path);
        widest.start_time = Some(u64::MAX);
        widest.poststart = Some(false);
        widest.pid_namespace = Some(false);
        widest.start_container = Some(u64::MAX);
        let others = serde_json::to_vec(&widest).unwrap().len() - "{}".len();
        assert_eq!(MAX_STATE_BYTES, MAX_CONFIG_BYTES + others);
    }

    /// A state's annotations take no more bytes than the config's text of them: serde_json escapes no character a
    /// config could hold in fewer bytes (RFC 8259 §7). Do: parse a config whose annotations use every escape and
    /// raw UTF-8, and write its state. Expect: the state's annotations no longer than the config's text of them.
    #[test]
    fn a_states_annotations_take_no_more_than_the_configs_text_of_them() {
        let config = r#"{"ociVersion":"1.3.0","root":{"path":"rootfs"},"annotations":{"k\u0001":"\"\\\/\b\f\n\r\t\u001féé","k":"v","k":"w"}}"#;
        let spec = crate::linux::oci::spec::Spec::parse(config.as_bytes()).unwrap();
        let key = r#""annotations":"#;
        let text = config.split_once(key).unwrap().1.strip_suffix('}').unwrap();
        let annotations = serde_json::to_vec(&spec.annotations).unwrap();
        assert!(
            annotations.len() <= text.len(),
            "{} > {}",
            annotations.len(),
            text.len()
        );
    }

    /// Do: a startContainer failure whose reason is longer than a pipe's atomic write, peeked from a FIFO,
    /// then peeked again. Expect: the reason, cut at a character's edge to fit, both times (nothing taken),
    /// and a taken start reads as none.
    #[test]
    fn a_start_failure_is_read_whole_without_being_taken() {
        let long = OciError::Os {
            op: "startContainer hook",
            why: "é".repeat(MAX_START_FAILURE),
        };
        let frame = start_failure(&long);
        assert!(frame.len() <= MAX_START_FAILURE, "{}", frame.len());
        let (r, w) = rustix::pipe::pipe_with(rustix::pipe::PipeFlags::CLOEXEC).unwrap();
        rustix::io::write(&w, &frame).unwrap();
        let first = peek_start(r.as_fd()).unwrap().unwrap();
        assert!(
            first.starts_with("startContainer hook failed: é"),
            "{first}"
        );
        assert_eq!(peek_start(r.as_fd()).unwrap(), Some(first));
        let (r, w) = rustix::pipe::pipe_with(rustix::pipe::PipeFlags::CLOEXEC).unwrap();
        rustix::io::write(&w, &[START_TAKEN]).unwrap();
        assert_eq!(peek_start(r.as_fd()).unwrap(), None);
    }

    /// CLAUDE.md §2. Do: run a hook that never reads its stdin and never ends by itself, with a state one byte more
    /// than a new pipe holds (`F_GETPIPE_SZ`, pipe(7): the hook's stdin is a new pipe), and a one-second timeout.
    /// Expect: refused for outliving its timeout, once that has passed: a write blocked for good would never return.
    #[test]
    fn a_hook_that_never_reads_its_stdin_ends_at_its_timeout() {
        let (probe, _) = rustix::pipe::pipe_with(rustix::pipe::PipeFlags::CLOEXEC).unwrap();
        let holds = rustix::pipe::fcntl_getpipe_size(&probe).unwrap();
        let mut state = State::new("c1", "/bundle");
        state
            .annotations
            .insert("big".to_owned(), "x".repeat(holds + 1));
        let started = Instant::now();
        let got = run("a hook", &[sh("exec sleep infinity", Some(1))], &state);
        assert!(
            matches!(&got, Err(OciError::Os { why, .. }) if why.contains("timeout")),
            "{got:?}"
        );
        assert!(
            started.elapsed() >= Duration::from_secs(1),
            "{:?}",
            started.elapsed()
        );
    }

    /// AC-T7.1.4; Ada's 2026-11-06 rule (no picked constants): the runner waits on the facts it needs, the hook
    /// taking its state and the hook's end, never on a poll interval. Do: run a hook that takes its state and
    /// then lives on a while. Expect: the runner woke at most once for each of those two facts, however long
    /// the hook lived. (Its 0.2 s is a fixture: any life longer than a poll interval tells a poll from a
    /// wait, since a poll wakes once an interval.)
    #[test]
    fn a_hook_is_waited_on_not_polled() {
        let state = State::new("c1", "/bundle");
        let before = wakes_on_this_thread();
        run(
            "a hook",
            &[sh("cat > /dev/null; exec sleep 0.2", None)],
            &state,
        )
        .unwrap();
        let wakes = wakes_on_this_thread() - before;
        assert!(wakes <= 2, "woke {wakes} times");
    }

    /// AC-T7.1.4, config.md:559-560; the lead's call of 2026-11-06 under Ada's rule (no picked constants): a hook
    /// that names no timeout has no deadline of the runtime's choosing, as runc's (`Command.Run`,
    /// libcontainer/configs/config.go:620-635: a nil `Timeout` waits for the command). Do: ask the deadline of a
    /// hook naming none and of one naming 5 s, and the budget of lists holding each. Expect: no deadline and no
    /// budget for the first, 5 s for the second.
    #[test]
    fn a_hook_naming_no_timeout_has_no_deadline() {
        let none = sh("true", None);
        let five = sh("true", Some(5));
        assert_eq!(timeout("a", &none).unwrap(), None);
        assert_eq!(timeout("a", &five).unwrap(), Some(Duration::from_secs(5)));
        assert_eq!(budget("a", &[five.clone(), none]).unwrap(), None);
        assert_eq!(budget("a", &[five.clone(), five]).unwrap(), Some(10));
    }

    /// config.md:592. Do: a hook that copies its stdin out, and one that fails. Expect: the state as
    /// JSON, and the failure named by its list.
    #[test]
    fn a_hook_is_given_the_state_and_a_failure_names_its_list() {
        let dir = tempfile::tempdir().unwrap();
        let out = dir.path().join("state");
        let state = State::new("c1", "/bundle");
        run(
            "a hook",
            &[sh(&format!("cat > {}", out.display()), None)],
            &state,
        )
        .unwrap();
        let seen: State = serde_json::from_slice(&std::fs::read(&out).unwrap()).unwrap();
        assert_eq!(seen, state);
        let got = run("createContainer hook", &[sh("exit 3", None)], &state);
        assert!(
            got.as_ref()
                .is_err_and(|e| e.to_string().starts_with("createContainer hook failed")),
            "{got:?}"
        );
    }

    /// config.md:556, :559-560. Do: parse specs whose hook has a relative path, a zero timeout, or
    /// startContainer timeouts that overflow. Expect: each refused by name; a spec with none of these is
    /// taken.
    #[test]
    fn a_hook_the_spec_forbids_is_refused_by_name() {
        let spec = |hooks: &str| {
            Spec::parse(
                format!(r#"{{"ociVersion":"1.3.0","root":{{"path":"rootfs"}},"hooks":{hooks}}}"#)
                    .as_bytes(),
            )
        };
        let max = i64::MAX;
        for (hooks, names) in [
            (r#"{"createContainer":[{"path":"sh"}]}"#, "createContainer"),
            (
                r#"{"startContainer":[{"path":"/bin/sh","timeout":0}]}"#,
                "above zero",
            ),
            (
                &format!(
                    r#"{{"startContainer":[{{"path":"/bin/sh","timeout":{max}}},{{"path":"/bin/sh","timeout":{max}}},{{"path":"/bin/sh","timeout":{max}}}]}}"#
                ),
                "overflow",
            ),
        ] {
            let got = spec(hooks);
            assert!(
                matches!(&got, Err(OciError::Config(why)) if why.contains(names)),
                "{hooks}: {got:?}"
            );
        }
        // 5 is a fixture: any timeout above zero is taken, and the budget is it.
        let ok = spec(r#"{"startContainer":[{"path":"/bin/sh","timeout":5}]}"#).unwrap();
        assert_eq!(
            budget("startContainer", &ok.hooks.start_container).unwrap(),
            Some(5)
        );
        assert!(ok.hooks.sync_init());
    }
}
