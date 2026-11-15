//! AC-T7.4.1 (ROOT item 3, the CPU-held scope's exec'd member): a managed scope's member that its watcher
//! holds execs between the hold's token capture and a later signal of that hold. XNU gives a process a new
//! audit-token pid version at exec, so the token names an image the member no longer runs. Each case drives
//! the watcher's own steps in their order (`apply`'s hold, `run_one`'s continue for a read, `stop_again`) on
//! a real member in the scope's group, beside its leader (the controller, as the fixture's), and expects
//! the same process (pid and birth) signalled, held as before, and its scope still Running. And one that
//! ends while held, or before the scope's end reaches it, is let go of, never taken for a process outside
//! the scope.
use super::scope_completion_tests::{Birth, unobserved_box};
use super::*;
use std::io::{BufRead as _, Write as _};
use std::os::unix::process::CommandExt as _;
use std::process::{Child, ChildStdin, Command, Stdio};

/// Longest a test waits for a fact it needs: a bound, so a member that never makes it true fails the
/// test instead of hanging it.
const PATIENCE: Duration = Duration::from_secs(30);

/// A held member: a shell that says it runs, and given a line execs a shell that says so and then waits on
/// its own stdin; in the group of its scope's leader, a shell waiting on its own.
struct Held {
    leader: Child,
    child: Child,
    stdin: ChildStdin,
    said: std::sync::mpsc::Receiver<String>,
    boxed: ProcessBox,
    pid: i32,
    started: u64,
}

impl Held {
    /// The leader started in a process group of its own and the scope's box built over it; the member
    /// started in its group and, once its first shell says it runs (past its own start's exec), the
    /// watcher's hold applied: stopped, its token captured from that shell.
    fn new() -> Self {
        let leader = Command::new("/bin/sh")
            .args(["-c", "read line"])
            .process_group(0)
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        let group = i32::try_from(leader.id()).unwrap();
        let led = sys::rusage(group).unwrap().unwrap().started;
        let mut child = Command::new("/bin/sh")
            .args([
                "-c",
                "echo ready; read line; exec /bin/sh -c 'echo execd; read more'",
            ])
            .process_group(group)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        let stdin = child.stdin.take().unwrap();
        let stdout = child.stdout.take().unwrap();
        let (sender, said) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            for line in std::io::BufReader::new(stdout).lines() {
                let Ok(line) = line else { return };
                if sender.send(line).is_err() {
                    return;
                }
            }
        });
        assert_eq!(said.recv_timeout(PATIENCE).unwrap(), "ready");
        let pid = i32::try_from(child.id()).unwrap();
        let started = sys::rusage(pid).unwrap().unwrap().started;
        let boxed = unobserved_box(&Birth {
            pid: group,
            started: led,
            session: 0,
        });
        let member = Member {
            pid: child.id(),
            started,
            footprint: 0,
            life: Life::Running,
            armed: false,
        };
        assert_eq!(
            boxed
                .state
                .apply(&[child.id()], &[], &[member])
                .unwrap()
                .held,
            1
        );
        let held = Self {
            leader,
            child,
            stdin,
            said,
            boxed,
            pid,
            started,
        };
        held.wait_until(sys::Run::Stopped);
        held
    }

    fn group(&self) -> i32 {
        i32::try_from(self.leader.id()).unwrap()
    }

    #[track_caller]
    fn wait_until(&self, run: sys::Run) {
        let deadline = Instant::now() + PATIENCE;
        loop {
            let now = sys::run_state(self.pid).unwrap();
            if now == Some(run) {
                return;
            }
            assert!(
                Instant::now() < deadline,
                "the member never reached {run:?}: {now:?}"
            );
            std::thread::park_timeout(Duration::from_millis(1));
        }
    }

    /// The watcher's group listing, with room for every process this kernel allows, as its own has.
    fn listing() -> Vec<i32> {
        storage(sys::max_processes().unwrap()).unwrap()
    }

    /// Its line, sent while it is held, then the watcher's continue for a read: it execs, and says so.
    fn exec_in_its_read(&mut self) {
        self.stdin.write_all(b"go\n").unwrap();
        let mut pids = Self::listing();
        let run = self
            .boxed
            .state
            .run_one(self.group(), &mut pids, 0)
            .unwrap()
            .expect("the held member runs for the read");
        assert_eq!(run.pid, self.pid);
        assert_eq!(self.said.recv_timeout(PATIENCE).unwrap(), "execd");
    }

    /// The same process, held by the watcher, and its scope still Running (not Uncertain).
    fn still_held(&self) {
        assert_eq!(
            sys::rusage(self.pid).unwrap().map(|usage| usage.started),
            Some(self.started),
            "the member isn't the same process"
        );
        assert!(self.boxed.state.held().unwrap().stopped.contains(&self.pid));
        let scope = self.boxed.state.vm.as_ref().unwrap();
        assert!(
            scope.watchable(self.boxed.state.id.as_ref()).unwrap(),
            "the scope left Running"
        );
    }
}

impl Drop for Held {
    fn drop(&mut self) {
        for child in [&mut self.child, &mut self.leader] {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

/// The member execs while it runs for its read; the watcher then stops it again with the hold's token:
/// expect it stopped, the same process, still held, and the scope Running.
#[test]
fn a_held_member_that_execs_in_its_read_is_stopped_again() {
    let mut held = Held::new();
    held.exec_in_its_read();
    let again = held.boxed.state.stop_again(held.pid);
    assert!(again.is_ok(), "{again:?}");
    held.wait_until(sys::Run::Stopped);
    held.still_held();
}

/// The member execs in its read and its stop lands only after the exec (as a stop posted while the
/// kernel runs an exec does): the watcher's next continue uses the hold's token. Expect it continued,
/// the same process, still held, and the scope Running.
#[test]
fn a_held_member_stopped_after_its_exec_is_continued_again() {
    let mut held = Held::new();
    held.exec_in_its_read();
    rustix::process::kill_process(
        rustix::process::Pid::from_raw(held.pid).unwrap(),
        rustix::process::Signal::STOP,
    )
    .unwrap();
    held.wait_until(sys::Run::Stopped);
    let mut pids = Held::listing();
    let run = held.boxed.state.run_one(held.group(), &mut pids, 0);
    assert!(run.is_ok(), "{:?}", run.map(|ran| ran.map(|ran| ran.pid)));
    held.wait_until(sys::Run::Live);
    held.still_held();
}

/// The held member ends (killed; its zombie unreaped, its parent this test, as the helper keeps a scope's
/// controller until finalize) and the watcher's read continues the held: expect no run, the member no
/// longer held, and the scope Running. A zombie is no process group's (`getpgid` finds no process), so a
/// signal fenced on the group would take it for a process outside the scope.
#[test]
fn a_held_member_that_ended_is_no_longer_held() {
    let held = Held::new();
    rustix::process::kill_process(
        rustix::process::Pid::from_raw(held.pid).unwrap(),
        rustix::process::Signal::KILL,
    )
    .unwrap();
    // Its zombie: proc_pid_rusage reads one (proc_pidinfo's run state finds none).
    let deadline = Instant::now() + PATIENCE;
    while !sys::rusage(held.pid)
        .unwrap()
        .is_some_and(|usage| usage.ended && usage.started == held.started)
    {
        assert!(Instant::now() < deadline, "the member never ended");
        std::thread::park_timeout(Duration::from_millis(1));
    }
    let mut pids = Held::listing();
    let run = held.boxed.state.run_one(held.group(), &mut pids, 0);
    assert!(
        matches!(run, Ok(None)),
        "{:?}",
        run.map(|ran| ran.map(|ran| ran.pid))
    );
    assert!(!held.boxed.state.held().unwrap().stopped.contains(&held.pid));
    let scope = held.boxed.state.vm.as_ref().unwrap();
    assert!(scope.watchable(held.boxed.state.id.as_ref()).unwrap());
}

/// The scope's end reaches a member that ended on its own after its token was read, and was reaped (as a
/// controller's child is when the controller ends first, its foreground child killed: run 11 of the
/// helper-flake sweep). The kill finds no process at its pid: expect it gone, nothing signalled, and the
/// scope still Running, never Uncertain (a member that has ended needs no end).
#[test]
fn a_member_reaped_before_its_kill_is_gone_and_the_scope_stays_certain() {
    let mut held = Held::new();
    let member = held
        .boxed
        .state
        .held()
        .unwrap()
        .stopped
        .get(&held.pid)
        .unwrap();
    held.child.kill().unwrap();
    held.child.wait().unwrap();
    let killed = held
        .boxed
        .state
        .signal_stopped(member, sys::MemberSignal::Kill);
    assert!(
        matches!(killed, Ok(None)),
        "{:?}",
        killed.map(|signalled| signalled.map(|member| member.pid))
    );
    let scope = held.boxed.state.vm.as_ref().unwrap();
    assert!(scope.watchable(held.boxed.state.id.as_ref()).unwrap());
}

/// A member listed by the watcher's read ends before its hold reaches it (its zombie unreaped): expect
/// no member to stop, and the scope still Running. The hold's capture of an exiting member is the same
/// race (task_name_for_pid finds its process but no task: NoRight), which a test can't stop inside.
#[test]
fn a_member_that_ended_before_its_hold_is_not_held_and_the_scope_stays_certain() {
    let mut held = Held::new();
    let member = Member {
        pid: u32::try_from(held.pid).unwrap(),
        started: held.started,
        footprint: 0,
        life: Life::Running,
        armed: false,
    };
    held.child.kill().unwrap();
    let deadline = Instant::now() + PATIENCE;
    while !sys::rusage(held.pid)
        .unwrap()
        .is_some_and(|usage| usage.ended)
    {
        assert!(Instant::now() < deadline, "the member never ended");
        std::thread::park_timeout(Duration::from_millis(1));
    }
    let stopped = held.boxed.state.stopped_member(held.pid, &[member]);
    assert!(
        matches!(stopped, Ok(None)),
        "{:?}",
        stopped.map(|member| member.map(|member| member.pid))
    );
    let scope = held.boxed.state.vm.as_ref().unwrap();
    assert!(scope.watchable(held.boxed.state.id.as_ref()).unwrap());
}

/// AC-T7.4.1 (the original of the helper's Control failure that a restarted node's preflight then refused:
/// "original native scope failure ... proc_signal_with_audittoken ... ESRCH"): the held member execs, so the
/// scope's end is refused by its hold's token (ESRCH) and captures a fresh one; the member then ends before
/// that fresh token's signal (ended and reaped here, inside the signal's seam, as a member exiting on its own
/// races the watcher). Do: end the member by its stale hold. Expect it gone, nothing more signalled, and the
/// scope still Running: a member its fresh token is refused for too has ended, the end the scope's signal
/// wanted, never a failure of the scope.
#[test]
fn a_member_that_ends_before_its_fresh_tokens_signal_is_gone_and_the_scope_stays_certain() {
    let mut held = Held::new();
    held.exec_in_its_read();
    let member = held
        .boxed
        .state
        .held()
        .unwrap()
        .stopped
        .get(&held.pid)
        .unwrap();
    let child = &mut held.child;
    let state = &held.boxed.state;
    let mut sends = 0;
    let killed = state.signal_stopped_by(member, sys::MemberSignal::Kill, |token, signal| {
        sends += 1;
        if sends == 2 {
            child.kill().unwrap();
            child.wait().unwrap();
        }
        sys::signal_member(token, signal)
    });
    assert_eq!(
        sends, 2,
        "the stale hold's token wasn't refused and a fresh one signalled"
    );
    assert!(
        matches!(killed, Ok(None)),
        "{:?}",
        killed.map(|signalled| signalled.map(|member| member.pid))
    );
    let scope = held.boxed.state.vm.as_ref().unwrap();
    assert!(
        scope.watchable(held.boxed.state.id.as_ref()).unwrap(),
        "the scope left Running"
    );
}

impl Held {
    /// The member as the watcher's read lists it.
    fn member(&self) -> Member {
        Member {
            pid: self.child.id(),
            started: self.started,
            footprint: 0,
            life: Life::Running,
            armed: false,
        }
    }
}

/// Joint plan step 14 (ROOT, 2026-11-13, the hold's hole): a held member whose stop didn't take runs past its charge on
/// its neighbours' CPU (Studio r214: one read live 76 µs after its SIGSTOP returned, and for the 5 s after), so the
/// watcher's next read of the hold finds it running and stops it again, by its hold's token, never waiting on it. Do
/// hold the member, then apply the hold again with the member's run state read live, as a stop that didn't take reads.
/// Expect it stopped again and still held, the re-stop counted, and the scope Running.
#[test]
fn w14_a_held_member_read_running_is_stopped_again_and_counted() {
    let held = Held::new();
    let live = |_: i32| -> std::io::Result<Option<sys::Run>> { Ok(Some(sys::Run::Live)) };
    let applied = held
        .boxed
        .state
        .apply_by(&[held.child.id()], &[], &[held.member()], live)
        .unwrap();
    assert_eq!(
        applied,
        Applied {
            held: 1,
            restopped: 1
        }
    );
    assert_eq!(held.boxed.state.status().unwrap().restops, 1);
    held.wait_until(sys::Run::Stopped);
    held.still_held();
}

/// Joint plan step 14 (ROOT, 2026-11-13, the hold's hole): a held member continued from outside its watcher (another
/// process's CONT, or the kernel's) reads running, and the watcher's next read stops it again. Do hold the member,
/// continue it from this test, then apply the hold again. Expect it found running, stopped again, still held, the
/// re-stop counted.
#[test]
fn w14_a_held_member_continued_from_elsewhere_is_stopped_again() {
    let held = Held::new();
    rustix::process::kill_process(
        rustix::process::Pid::from_raw(held.pid).unwrap(),
        rustix::process::Signal::CONT,
    )
    .unwrap();
    held.wait_until(sys::Run::Live);
    let applied = held
        .boxed
        .state
        .apply(&[held.child.id()], &[], &[held.member()])
        .unwrap();
    assert_eq!(applied.restopped, 1, "the running held member wasn't found");
    held.wait_until(sys::Run::Stopped);
    assert_eq!(held.boxed.state.status().unwrap().restops, 1);
    held.still_held();
}
