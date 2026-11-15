//! The box-user backend end to end, without root (HEX-MAC-16): a `MacRuntime` in box-user mode, driven
//! through the one `Runtime` trait, reaches a real daemon over a real Unix socket, and the daemon's
//! privileged spawn/arm/kill are a stand-in (no setuid, no ledger — those are measured as root by
//! `bench/native/macos/boxlab.sh`). It exercises the node side: mode detection, create→acquire a pool
//! user, exec→start as that user (stdio by `SCM_RIGHTS`), inspect, list, delete, and that inspect reports
//! box-user membership.

use std::collections::BTreeMap;
use std::io::{self, BufRead, BufReader};
use std::num::NonZeroU64;
use std::os::fd::{AsFd, OwnedFd};
use std::process::{Child, Command, Stdio as Piped};
use std::sync::{Arc, Mutex, mpsc};
use std::thread;
use std::time::Duration;

use hex_api::{
    Bounds, BoxId, BoxSpec, BoxState, ClassSpec, Exit, Membership, Proc, Program, Refusal, Runtime,
    Stdio,
};
use hex_core::helper::START_FDS;
use hex_core::helper::managed::response::Estimate;

use super::MacRuntime;
use super::helper_daemon::{self, Daemon, Member, Privileged};

/// How long a test waits for a fact before calling it a hang: twice the slowest recorded run of the tests
/// that wait (19.753 s, a kiln pacing stall, which stops a whole run tree and so can meet any wait;
/// bench/native/macos/records/test-waits-macos.txt). A guard only ends a hang; a passing test waits on its fact.
const HANG_GUARD: Duration = Duration::from_millis(39_507);

/// A stand-in for the daemon's privileged backend: it tracks a box's members in memory instead of
/// spawning or arming real processes, so the node→daemon path runs without root or side effects.
#[derive(Default)]
struct Sim {
    inner: Arc<Mutex<SimState>>,
}

#[derive(Default)]
struct SimState {
    next: i32,
    members: Vec<Member>,
}

impl Privileged for Sim {
    fn spawn(
        &self,
        _uid: u32,
        _program: &Program,
        fds: [OwnedFd; 3],
        _leader: bool,
        _cap_mib: u32,
        _nproc: u32,
    ) -> io::Result<i32> {
        drop(fds);
        let mut s = self.inner.lock().expect("sim lock");
        s.next += 1;
        let pid = 2000 + s.next;
        s.members.push(Member { pid, footprint: 0 });
        Ok(pid)
    }

    fn watch(&self, pid: i32) -> io::Result<Box<dyn Proc>> {
        Ok(Box::new(SimProc {
            pid,
            state: Arc::clone(&self.inner),
        }))
    }

    fn watch_members(&self, _pids: &[i32], _uid: u32) -> io::Result<Vec<i32>> {
        Ok(Vec::new())
    }

    fn next_member_event(&self) -> io::Result<Vec<u32>> {
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "the stand-in posts no member events",
        ))
    }

    fn next_crossing(&self) -> io::Result<()> {
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "the stand-in has no exception port",
        ))
    }

    fn arm(&self, _pid: i32, _cap_mib: u32) -> io::Result<()> {
        Ok(())
    }

    fn list(&self, _uid: u32) -> io::Result<Vec<Member>> {
        Ok(self.inner.lock().expect("sim lock").members.clone())
    }

    /// The stand-in's user is empty once its KILL has cleared its members.
    fn empties_within(&self, _uid: u32, _slice: Duration) -> Result<bool, Refusal> {
        Ok(self.inner.lock().expect("sim lock").members.is_empty())
    }

    fn kill(&self, _uid: u32, signal: i32) -> io::Result<usize> {
        let mut s = self.inner.lock().expect("sim lock");
        let n = s.members.len();
        if signal == 9 {
            s.members.clear();
        }
        Ok(n)
    }
}

/// The stand-in's watch: a program has ended (by KILL, the stand-in's only end) once it is no member.
struct SimProc {
    pid: i32,
    state: Arc<Mutex<SimState>>,
}

impl Proc for SimProc {
    fn pid(&self) -> u32 {
        u32::try_from(self.pid).unwrap_or(0)
    }

    fn try_wait(&mut self) -> Result<Option<Exit>, Refusal> {
        let s = self.state.lock().expect("sim lock");
        if s.members.iter().any(|m| m.pid == self.pid) {
            Ok(None)
        } else {
            Ok(Some(Exit::Signal(9)))
        }
    }

    fn wait_for(&mut self, _timeout: Duration) -> Result<Option<Exit>, Refusal> {
        self.try_wait()
    }

    /// The stand-in's KILL: the program is no member from then on.
    fn kill(&mut self) -> Result<(), Refusal> {
        let mut s = self.state.lock().expect("sim lock");
        s.members.retain(|m| m.pid != self.pid);
        Ok(())
    }

    fn streams(&mut self) -> Result<hex_api::Given, Refusal> {
        Ok(hex_api::Given::default())
    }
}

/// HEX-MAC-16: a box-user MacRuntime drives a box through the helper over a real socket.
#[test]
fn box_user_mode_routes_through_the_helper() {
    let dir = tempfile::tempdir().expect("temp dir");
    let sock = dir.path().join("helper.sock");
    // Bind before connecting, so the node's connect can't race the listener.
    let listener = helper_daemon::bind(&sock).expect("bind the helper socket");
    let server = thread::spawn(move || {
        let daemon = Daemon::new(Sim::default(), vec![7000, 7001], 64);
        let _ = daemon.serve_listener(listener.as_fd());
    });

    let rate = NonZeroU64::new(256 << 30).expect("nonzero");
    let lateness =
        hex_core::watch::lateness_seed(&super::machine_model().expect("this Mac's model"));
    let rt = MacRuntime::detect(rate, rate, lateness, &sock).expect("detect box-user mode");
    let id = BoxId::new("b1").expect("box id");

    let spec = BoxSpec {
        route: None,
        allocation: None,
        devices: Vec::new(),
        id: id.clone(),
        bounds: Bounds {
            memory_max: Some(256 << 20),
            ..Bounds::default()
        },
        class: ClassSpec::Process,
        owner: None,
        band: 0,
    };
    let created = rt.create(spec).expect("create");
    assert_eq!(created.membership, Some(Membership::BoxUser));
    assert_eq!(created.state, BoxState::Empty);

    let program = Program {
        args: vec!["/bin/true".to_owned()],
        env: vec![],
        cwd: None,
        user: None,
    };
    let proc = rt.exec(&id, program, Stdio::null()).expect("exec");
    assert!(proc.pid() > 2000, "the sim's pid came back");

    let info = rt.inspect(&id).expect("inspect");
    assert_eq!(info.membership, Some(Membership::BoxUser));
    assert_eq!(info.state, BoxState::Running);
    assert_eq!(rt.list().expect("list").len(), 1);

    rt.delete(&id).expect("delete");
    assert!(matches!(rt.inspect(&id), Err(Refusal::NoSuchBox(_))));

    drop(rt);
    // The daemon thread parks on accept once the node disconnects; it ends with the test process.
    drop(server);
}

/// A backend whose programs are real but children of a shell of their own, never of this process: as a
/// box-user program is the root helper's child, not the node's. Each spawn starts
/// `sh -c 'exec 3<&0; "$@" <&3 3<&- & exec 3<&-; echo $!; wait $!; echo $?'`, reads the program's pid, and a thread
/// reads its status. The program's stdin is the one the node passed, redirected by name: a shell gives a program it
/// starts in the background with no stdin of its own the null device (POSIX's asynchronous lists, without job
/// control).
#[derive(Default)]
struct Shells {
    ends: Mutex<BTreeMap<i32, mpsc::Receiver<Exit>>>,
    live: Mutex<Vec<i32>>,
}

struct ShellProc {
    pid: i32,
    end: mpsc::Receiver<Exit>,
    /// Its own watch of the program's exit, as the real helper's `BoxProc` holds one.
    kq: super::sys::Kqueue,
    exiting: bool,
    events: Vec<super::sys::Event>,
}

impl Proc for ShellProc {
    fn pid(&self) -> u32 {
        u32::try_from(self.pid).unwrap_or(0)
    }

    /// As the real one: take the exit event if its watch holds it, then the end, which follows it at once
    /// (here the shell's status line; there the zombie), up to the hang guard.
    fn try_wait(&mut self) -> Result<Option<Exit>, Refusal> {
        if !self.exiting {
            self.kq
                .wait(None, Some(Duration::ZERO), &mut self.events)
                .map_err(|e| Refusal::os("kevent", e))?;
            let me = self.pid;
            self.exiting = self
                .events
                .iter()
                .any(|e| matches!(e, super::sys::Event::Exit { pid, .. } if *pid == me));
        }
        if self.exiting {
            Ok(self.end.recv_timeout(HANG_GUARD).ok())
        } else {
            Ok(self.end.try_recv().ok())
        }
    }

    fn wait_for(&mut self, _timeout: Duration) -> Result<Option<Exit>, Refusal> {
        self.try_wait()
    }

    /// KILL to the shell by its pid, as the real program's own kill sends it.
    fn kill(&mut self) -> Result<(), Refusal> {
        let pid = rustix::process::Pid::from_raw(self.pid).ok_or(Refusal::Invalid {
            what: "pid",
            why: format!("{} isn't a process id", self.pid),
        })?;
        match rustix::process::kill_process(pid, rustix::process::Signal::KILL) {
            Ok(()) | Err(rustix::io::Errno::SRCH) => Ok(()),
            Err(e) => Err(Refusal::os("kill", e)),
        }
    }

    fn streams(&mut self) -> Result<hex_api::Given, Refusal> {
        Ok(hex_api::Given::default())
    }
}

impl Privileged for Shells {
    fn spawn(
        &self,
        _uid: u32,
        program: &Program,
        fds: [OwnedFd; START_FDS],
        _leader: bool,
        _cap_mib: u32,
        _nproc: u32,
    ) -> io::Result<i32> {
        // The program's stdin is the one the node passed, as the helper's `dup2` makes it.
        let [stdin, _, _] = fds;
        let mut shell: Child = Command::new("/bin/sh")
            .args([
                "-c",
                "exec 3<&0; \"$@\" <&3 3<&- >/dev/null 2>&1 & exec 3<&-; echo $!; wait $!; echo $?",
                "sh",
            ])
            .args(&program.args)
            .stdin(Piped::from(stdin))
            .stdout(Piped::piped())
            .spawn()?;
        let mut lines = BufReader::new(shell.stdout.take().expect("piped")).lines();
        let pid: i32 = lines
            .next()
            .transpose()?
            .and_then(|l| l.trim().parse().ok())
            .ok_or_else(|| io::Error::other("the shell named no pid"))?;
        let (tx, rx) = mpsc::channel();
        thread::spawn(move || {
            let status: i32 = lines
                .next()
                .and_then(Result::ok)
                .and_then(|l| l.trim().parse().ok())
                .unwrap_or(-1);
            let _ = shell.wait();
            let _ = tx.send(if status > 128 {
                Exit::Signal(status - 128)
            } else {
                Exit::Code(status)
            });
        });
        self.ends.lock().unwrap().insert(pid, rx);
        self.live.lock().unwrap().push(pid);
        Ok(pid)
    }

    fn watch(&self, pid: i32) -> io::Result<Box<dyn Proc>> {
        let end = self
            .ends
            .lock()
            .unwrap()
            .remove(&pid)
            .ok_or_else(|| io::Error::other("no such program"))?;
        let kq = super::sys::Kqueue::new()?;
        let exiting = kq.watch(&[pid], 0)?.contains(&pid);
        Ok(Box::new(ShellProc {
            pid,
            end,
            kq,
            exiting,
            events: Vec::new(),
        }))
    }

    fn watch_members(&self, _pids: &[i32], _uid: u32) -> io::Result<Vec<i32>> {
        Ok(Vec::new())
    }

    fn next_member_event(&self) -> io::Result<Vec<u32>> {
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "the stand-in posts no member events",
        ))
    }

    fn next_crossing(&self) -> io::Result<()> {
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "the stand-in has no exception port",
        ))
    }

    fn arm(&self, _pid: i32, _cap_mib: u32) -> io::Result<()> {
        Ok(())
    }

    fn list(&self, _uid: u32) -> io::Result<Vec<Member>> {
        Ok(Vec::new())
    }

    fn kill(&self, _uid: u32, _signal: i32) -> io::Result<usize> {
        Ok(0)
    }

    /// The stand-in's programs aren't its user's (they run as this test's), so its user holds none: as its listing.
    fn empties_within(&self, _uid: u32, _slice: Duration) -> Result<bool, Refusal> {
        Ok(true)
    }
}

/// HEX-RT-3, AC-T7.1.1 (box-user mode, end to end): a node in box-user mode, through a daemon on a real
/// socket, starts programs that are another process's children, as the root helper's are. Do run one that
/// waits on its stdin, then one that exits 3; expect the first not ended until its stdin closes and both
/// ends as they were,
/// never an end the node made up because it couldn't reap a program it doesn't parent.
#[test]
fn a_box_user_programs_end_is_its_own_not_unknown() {
    let dir = tempfile::tempdir().expect("temp dir");
    let sock = dir.path().join("helper.sock");
    let listener = helper_daemon::bind(&sock).expect("bind");
    thread::spawn(move || {
        let daemon = Daemon::new(Shells::default(), vec![7000], 64);
        let _ = daemon.serve_listener(listener.as_fd());
    });
    let rt = MacRuntime::detect(
        NonZeroU64::new(1 << 40).unwrap(),
        NonZeroU64::new(1_000_000).unwrap(),
        hex_core::watch::lateness_seed(&super::machine_model().expect("this Mac's model")),
        &sock,
    )
    .expect("a box-user runtime");
    let id = BoxId::new("bu-end").unwrap();
    rt.create(BoxSpec {
        route: None,
        allocation: None,
        devices: Vec::new(),
        id: id.clone(),
        bounds: Bounds::default(),
        class: ClassSpec::Process,
        owner: None,
        band: 0,
    })
    .expect("create");
    let sh = |script: &str| Program {
        args: vec!["/bin/sh".to_owned(), "-c".to_owned(), script.to_owned()],
        env: vec![],
        cwd: None,
        user: None,
    };
    // A program that runs until the test closes its stdin: running is a fact here, not a delay.
    let (stdin, hold) = std::io::pipe().expect("pipe");
    let mut held = rt
        .exec(
            &id,
            sh("read _; exit 0"),
            Stdio {
                stdin: Some(stdin),
                ..Stdio::null()
            },
        )
        .expect("exec");
    assert_eq!(
        held.try_wait().unwrap(),
        None,
        "a running program read as ended"
    );
    drop(hold);
    assert_eq!(held.wait_for(HANG_GUARD).unwrap(), Some(Exit::Code(0)));
    let mut three = rt.exec(&id, sh("exit 3"), Stdio::null()).expect("exec");
    assert_eq!(three.wait_for(HANG_GUARD).unwrap(), Some(Exit::Code(3)));
}

/// HEX-RT-16 (box-user mode, end to end; the lead, 2026-11-09): a node in box-user mode kills one of a box's
/// programs through its own Proc, by the helper's hand (the program is the pool user's, which the node may
/// not signal), and the box's other program runs on. Do run two programs that run until they are killed (each
/// bounded by the hang guard), kill the first through its Proc twice, then the second; expect the first ended
/// by Signal(9) and its second kill Ok, the second still running after the first's end, then ended by its own
/// kill.
#[test]
fn a_box_user_program_is_killed_alone_through_its_proc() {
    let dir = tempfile::tempdir().expect("temp dir");
    let sock = dir.path().join("helper.sock");
    let listener = helper_daemon::bind(&sock).expect("bind");
    thread::spawn(move || {
        let daemon = Daemon::new(Shells::default(), vec![7000], 64);
        let _ = daemon.serve_listener(listener.as_fd());
    });
    let rt = MacRuntime::detect(
        NonZeroU64::new(1 << 40).unwrap(),
        NonZeroU64::new(1_000_000).unwrap(),
        hex_core::watch::lateness_seed(&super::machine_model().expect("this Mac's model")),
        &sock,
    )
    .expect("a box-user runtime");
    let id = BoxId::new("bu-kill").unwrap();
    rt.create(BoxSpec {
        route: None,
        allocation: None,
        devices: Vec::new(),
        id: id.clone(),
        bounds: Bounds::default(),
        class: ClassSpec::Process,
        owner: None,
        band: 0,
    })
    .expect("create");
    let guard = HANG_GUARD.as_secs().to_string();
    let until_killed = || Program {
        args: vec!["/bin/sleep".to_owned(), guard.clone()],
        env: vec![],
        cwd: None,
        user: None,
    };
    let mut first = rt
        .exec(&id, until_killed(), Stdio::null())
        .expect("exec the first");
    let mut second = rt
        .exec(&id, until_killed(), Stdio::null())
        .expect("exec the second");
    first.kill().expect("kill the first");
    assert_eq!(first.wait_for(HANG_GUARD).unwrap(), Some(Exit::Signal(9)));
    first.kill().expect("a second kill of the ended one");
    assert_eq!(
        second.try_wait().unwrap(),
        None,
        "the box's other program ended with the first"
    );
    second.kill().expect("kill the second");
    assert_eq!(second.wait_for(HANG_GUARD).unwrap(), Some(Exit::Signal(9)));
}

/// CLAUDE.md §5 (the box helper's default process cap is the kernel's, not picked): do read the per-user
/// process limit; expect what `sysctl kern.maxprocperuid` prints, the limit macOS itself holds each user to
/// (`bsd/kern/kern_resource.c:169-170`), so a box's user gets what any user of this machine gets.
#[test]
fn the_default_box_process_cap_is_the_kernels_per_user_limit() {
    let out = std::process::Command::new("/usr/sbin/sysctl")
        .args(["-n", "kern.maxprocperuid"])
        .output()
        .expect("sysctl");
    let want: usize = String::from_utf8_lossy(&out.stdout)
        .trim()
        .parse()
        .expect("a number");
    assert_eq!(super::sys::max_processes_per_uid().expect("read"), want);
}

/// HEX-RT-3 (no wait bound to pick): do watch a running child, then end it; expect `try_wait` to answer
/// it runs at once while it does, and its end at once after its exit event, with no wait between (it
/// takes the event its watch holds and then reaps the zombie the same exit path makes), so the box
/// helper answers a node's `Wait` without bounding a wait.
#[test]
fn try_wait_answers_at_once_running_or_ended() {
    let mut child = Command::new("/bin/sh")
        .args(["-c", "read _; exit 3"])
        .stdin(Piped::piped())
        .spawn()
        .expect("spawn");
    let pid = i32::try_from(child.id()).expect("pid");
    let mut p = super::proc::BoxProc::watch(pid, None).expect("watch");
    assert_eq!(
        p.try_wait().expect("try_wait"),
        None,
        "a running child read as ended"
    );
    drop(child.stdin.take());
    // Wait on the fact: the child's exit event, on a kqueue of the test's own.
    let kq = super::sys::Kqueue::new().expect("kqueue");
    let mut events = Vec::new();
    if !kq.watch(&[pid], 0).expect("watch").contains(&pid) {
        while !events
            .iter()
            .any(|e| matches!(e, super::sys::Event::Exit { pid: p, .. } if *p == pid))
        {
            kq.wait(None, Some(HANG_GUARD), &mut events).expect("wait");
        }
    }
    assert_eq!(p.try_wait().expect("try_wait"), Some(Exit::Code(3)));
    // `try_wait` reaped it; this wait finds no child left (ECHILD) and leaves no zombie either way.
    let _ = child.wait();
}

/// A box-user program runs as its pool user, which the unprivileged node may not signal, and XNU attaches
/// `NOTE_EXITSTATUS` only for a parent or one that may (`kern_event.c:1116-1133`, `EACCES` else), so the
/// node's watch of it asks for `NOTE_EXIT` alone (HEX-MAC-16, HEX-RT-3). launchd (pid 1) stands in for a
/// process this one may not signal. Do watch it with the members' status watch, then for its end alone;
/// expect the first refused (unless root) and the second taken, launchd not ended.
#[test]
fn a_process_this_one_may_not_signal_is_watched_for_its_end_alone() {
    let kq = super::sys::Kqueue::new().expect("kqueue");
    if !rustix::process::geteuid().is_root() {
        let refused = kq
            .watch(&[1], 0)
            .expect_err("a status watch of launchd taken unprivileged");
        assert_eq!(refused.raw_os_error(), Some(libc::EACCES));
    }
    assert!(!kq.watch_end(1).expect("an end watch of launchd"));
}

/// Ada's rule (2026-11-06): no picked number stands in for the kernel's. A process or user listing is sized
/// by `kern.maxproc`; when that can't be read, the listing is refused naming it (CLAUDE.md §3), never sized
/// by a guess (the 4096 step 1's class and the helper fell back to). Do size a listing from a failed read
/// and from a read of 7; expect an error naming kern.maxproc, and room for 7.
#[test]
fn a_listing_is_sized_by_kern_maxproc_or_refused() {
    let failed = super::sys::listing_from(Err(std::io::Error::from_raw_os_error(libc::ENOENT)));
    let refused = failed.expect_err("a listing sized by a guess");
    assert!(refused.to_string().contains("kern.maxproc"), "{refused}");
    let sized = super::sys::listing_from(Ok(7)).expect("a listing for 7");
    assert!(sized.capacity() >= 7 && sized.is_empty());
}

/// HEX-MAC-1, CLAUDE.md §3: a member killed at its fatal cap is counted once, by whichever of its waiter and
/// the watcher reads its zombie's peak. The watcher reads an exit event's member when it wakes, which can be
/// after the member's waiter has reaped it, and then has no peak to decide by; the waiter read the peak
/// before its reap. Do tell a box that an armed member died by SIGKILL, first with no peak (the watcher, the
/// member reaped under it) and then with its peak at the cap (its waiter); expect one memory kill.
#[test]
fn a_cap_kill_the_watcher_finds_reaped_is_counted_by_its_waiter() {
    let model = super::machine_model().expect("this Mac's model");
    let rates = hex_core::watch::seed(&model);
    let rt = MacRuntime::new(
        rates.bytes,
        rates.processes,
        hex_core::watch::lateness_seed(&model),
    )
    .expect("a macOS runtime");
    let id = BoxId::new("mac-cap-kill-reaped").expect("an id");
    let bound: u64 = 256 << 20;
    let _ = rt.delete(&id);
    rt.create(BoxSpec {
        route: None,
        allocation: None,
        devices: Vec::new(),
        id: id.clone(),
        bounds: Bounds {
            memory_max: Some(bound),
            ..Bounds::default()
        },
        class: ClassSpec::Process,
        owner: None,
        band: 0,
    })
    .expect("create");
    // A member that lives no longer than the hang guard, armed with the box's cap as every member is.
    let guard = HANG_GUARD.as_secs().to_string();
    let p = rt
        .exec(&id, Program::of(&["/bin/sleep", &guard]), Stdio::null())
        .expect("exec");
    let pid = i32::try_from(p.pid()).expect("a pid");
    let cap = super::sys::jetsam_cap_bytes(bound).expect("a cap for the bound");
    let members = rt.get(&id).expect("the box").members();
    members
        .ended(pid, true, None)
        .expect("the watcher's account");
    members
        .ended(pid, true, Some(cap))
        .expect("the waiter's account");
    let kills = rt.usage(&id).expect("usage").oom_kills;
    rt.delete(&id).expect("delete");
    drop(p);
    assert_eq!(
        kills, 1,
        "a cap kill the watcher found reaped went uncounted"
    );
}

/// Waits until process `pid` is in run state `want`, as the kernel reads it, up to the hang guard.
fn wait_for_run_state(pid: i32, want: super::sys::Run) {
    let deadline = std::time::Instant::now() + HANG_GUARD;
    while super::sys::run_state(pid).expect("run state") != Some(want) {
        assert!(
            std::time::Instant::now() < deadline,
            "{pid} never became {want:?}"
        );
        thread::yield_now();
    }
}

/// CLAUDE.md §3, a stop isn't an end: macOS answers `waitid`'s `WEXITED` with a stopped child's report too
/// (measured on adas-mac-studio-3, Mac16,9, macOS 26.2, 2026-11-08: a stopped child read by
/// `waitid(P_PID, WEXITED | WNOHANG)`, the reap taking the report), and a program's waiter took that report
/// for its end, `Exit::Unknown`, while it lived on stopped. Do stop a running program its waiter watches, ask
/// for its end, continue it and ask again, then end it; expect it running both times, then its own exit.
#[test]
fn a_stopped_program_is_running_to_its_waiter_not_ended() {
    let mut child = Reaped(
        Command::new("/bin/sh")
            .args(["-c", "read _; exit 3"])
            .stdin(Piped::piped())
            .spawn()
            .expect("spawn"),
    );
    let pid = i32::try_from(child.0.id()).expect("pid");
    let raw = rustix::process::Pid::from_raw(pid).expect("a pid");
    let mut p = super::proc::BoxProc::watch(pid, None).expect("watch");
    rustix::process::kill_process(raw, rustix::process::Signal::STOP).expect("stop");
    wait_for_run_state(pid, super::sys::Run::Stopped);
    assert_eq!(
        p.try_wait().expect("try_wait"),
        None,
        "a stopped program read as ended"
    );
    rustix::process::kill_process(raw, rustix::process::Signal::CONT).expect("continue");
    wait_for_run_state(pid, super::sys::Run::Live);
    assert_eq!(
        p.try_wait().expect("try_wait"),
        None,
        "a continued program read as ended"
    );
    drop(child.0.stdin.take());
    assert_eq!(p.wait_for(HANG_GUARD).expect("wait"), Some(Exit::Code(3)));
}

/// A child a test started, killed and reaped when the test ends, a failed assert included, so a stopped
/// child isn't left holding the test's stdio open.
struct Reaped(Child);

impl Drop for Reaped {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

/// The graded hold's sign of a run (the lead, 2026-11-09): a program asleep has no thread running or waiting
/// for a core, so a run of it shows it isn't growing; a spinning one has, so a run of it that grew nothing and
/// ran no code of its own shows nothing. Do read the threads of a sleeping child, then of a spinning one; expect
/// none runnable, then one.
#[test]
fn a_sleeping_program_has_no_runnable_thread_and_a_spinning_one_has() {
    let sleeper = Reaped(
        Command::new("/bin/sleep")
            .arg(HANG_GUARD.as_secs().to_string())
            .spawn()
            .expect("spawn sleep"),
    );
    let spinner = Reaped(
        Command::new("/usr/bin/yes")
            .stdout(Piped::null())
            .spawn()
            .expect("spawn yes"),
    );
    for (child, want) in [(&sleeper, false), (&spinner, true)] {
        let pid = i32::try_from(child.0.id()).expect("a pid");
        let deadline = std::time::Instant::now() + HANG_GUARD;
        while super::sys::task(pid).expect("task").map(|t| t.runnable) != Some(want) {
            assert!(
                std::time::Instant::now() < deadline,
                "{pid} never read runnable {want}"
            );
            thread::yield_now();
        }
    }
}

/// The lead's call (2026-11-08): a box's watcher continues only the members it held, so a member the box's
/// own programs stopped stays stopped. Do stop a box's member from outside the watcher, as one of the box's
/// programs would, then start a second, which the box holds until its watcher's next read continues it;
/// expect the second continued and the first still stopped.
#[test]
fn a_member_its_own_programs_stopped_stays_stopped_when_the_watcher_continues_the_box() {
    let model = super::machine_model().expect("this Mac's model");
    let rates = hex_core::watch::seed(&model);
    let rt = MacRuntime::new(
        rates.bytes,
        rates.processes,
        hex_core::watch::lateness_seed(&model),
    )
    .expect("a macOS runtime");
    let id = BoxId::new("mac-own-stop").expect("an id");
    let _ = rt.delete(&id);
    rt.create(BoxSpec {
        route: None,
        allocation: None,
        devices: Vec::new(),
        id: id.clone(),
        bounds: Bounds {
            memory_max: Some(256 << 20),
            ..Bounds::default()
        },
        class: ClassSpec::Process,
        owner: None,
        band: 0,
    })
    .expect("create");
    // Members that live no longer than the hang guard.
    let guard = HANG_GUARD.as_secs().to_string();
    let first = rt
        .exec(&id, Program::of(&["/bin/sleep", &guard]), Stdio::null())
        .expect("exec the first");
    let first_pid = i32::try_from(first.pid()).expect("a pid");
    rustix::process::kill_process(
        rustix::process::Pid::from_raw(first_pid).expect("a pid"),
        rustix::process::Signal::STOP,
    )
    .expect("stop the first");
    wait_for_run_state(first_pid, super::sys::Run::Stopped);
    let second = rt
        .exec(&id, Program::of(&["/bin/sleep", &guard]), Stdio::null())
        .expect("exec the second");
    let second_pid = i32::try_from(second.pid()).expect("a pid");
    // Only the watcher's read continues the second, which the box held from its start.
    wait_for_run_state(second_pid, super::sys::Run::Live);
    let first_now = super::sys::run_state(first_pid).expect("run state");
    rt.delete(&id).expect("delete");
    drop((first, second));
    assert_eq!(
        first_now,
        Some(super::sys::Run::Stopped),
        "the watcher continued a member the box's own programs stopped"
    );
}

/// c-route's finding (2026-11-07): XNU's EVFILT_PROC reports only what follows its attach and doesn't refuse
/// a zombie, so a member that ends between the watcher's listing and its watch posts no exit event, and a box
/// left to its events would wait on a change that already happened. Do end a child (a zombie, not reaped),
/// then watch it as the watcher watches a new member; expect the watch to report it ended.
#[test]
fn a_member_that_ended_before_its_watch_is_reported() {
    let mut child = Command::new("/usr/bin/true").spawn().expect("spawn true");
    let pid = i32::try_from(child.id()).expect("a pid");
    let deadline = std::time::Instant::now() + HANG_GUARD;
    while !super::sys::rusage(pid)
        .expect("rusage")
        .is_some_and(|r| r.ended)
    {
        assert!(std::time::Instant::now() < deadline, "true never ended");
        thread::yield_now();
    }
    let kq = super::sys::Kqueue::new().expect("kqueue");
    let ended = super::process::watch_members(&kq, &[pid]).expect("watch");
    let _ = child.wait();
    assert_eq!(
        ended,
        vec![pid],
        "a member ended before its watch wasn't reported"
    );
}

/// The variable under which the descriptor check runs, in a test process of its own.
const DESCRIPTORS_ALONE: &str = "HEX_MACOS_DESCRIPTORS_ALONE";

/// A process box of `rt`'s, made for the descriptor check, its program `script` in `/bin/sh` given `stdio`.
fn descriptors_box(rt: &MacRuntime, name: &str, stdio: Stdio) -> Box<dyn Proc> {
    let id = BoxId::new(name).expect("an id");
    let _ = rt.delete(&id);
    rt.create(BoxSpec {
        route: None,
        allocation: None,
        devices: Vec::new(),
        id: id.clone(),
        bounds: Bounds::default(),
        class: ClassSpec::Process,
        owner: None,
        band: 0,
    })
    .expect("create");
    rt.exec(
        &id,
        Program {
            args: vec![
                "/bin/sh".to_owned(),
                "-c".to_owned(),
                "read _; echo said".to_owned(),
            ],
            env: vec![],
            cwd: None,
            user: None,
        },
        stdio,
    )
    .expect("exec")
}

/// Ada and ROOT (2026-11-12), joint plan step 12, CLAUDE.md §12: no descriptor the node's process makes is left
/// without close-on-exec, so a program started any way, a Shards image VM's run host among them, holds none of the
/// node's. Do run a process box's program through the macOS runtime, rootless and through a box helper (the daemon
/// the tests stand in), its stdin and stdout pipes of this process's, in a test process of its own (other tests'
/// descriptors aren't this one's to judge). Expect: no descriptor past stdio that this process holds now and didn't
/// before is without close-on-exec, while the programs run and their watches are live.
#[test]
fn every_descriptor_the_runtime_makes_is_close_on_exec() {
    if std::env::var_os(DESCRIPTORS_ALONE).is_none() {
        let alone = Command::new(std::env::current_exe().expect("this test's binary"))
            .args([
                "--exact",
                "macos::tests::every_descriptor_the_runtime_makes_is_close_on_exec",
                "--test-threads=1",
            ])
            .env(DESCRIPTORS_ALONE, "1")
            .output()
            .expect("the check, alone");
        assert!(
            alone.status.success(),
            "{}{}",
            String::from_utf8_lossy(&alone.stdout),
            String::from_utf8_lossy(&alone.stderr)
        );
        return;
    }
    let me = i32::try_from(std::process::id()).expect("a pid");
    // What this process held before, inherited from what started it: not the runtime's.
    let before = super::sys::inheritable_descriptors(me).expect("this process's descriptors");
    let model = super::machine_model().expect("this Mac's model");
    let rates = hex_core::watch::seed(&model);
    let rootless = MacRuntime::new(
        rates.bytes,
        rates.processes,
        hex_core::watch::lateness_seed(&model),
    )
    .expect("a macOS runtime");
    let dir = tempfile::tempdir().expect("temp dir");
    let sock = dir.path().join("helper.sock");
    let listener = helper_daemon::bind(&sock).expect("bind");
    thread::spawn(move || {
        let daemon = Daemon::new(Shells::default(), vec![7000], 64);
        let _ = daemon.serve_listener(listener.as_fd());
    });
    let helped = MacRuntime::detect(
        NonZeroU64::new(1 << 40).unwrap(),
        NonZeroU64::new(1_000_000).unwrap(),
        hex_core::watch::lateness_seed(&model),
        &sock,
    )
    .expect("a box-user runtime");
    let mut running = Vec::new();
    for (rt, name) in [(&rootless, "mac-fds-rootless"), (&helped, "mac-fds-helped")] {
        let (stdin, hold) = std::io::pipe().expect("pipe");
        let (said, stdout) = std::io::pipe().expect("pipe");
        let program = descriptors_box(
            rt,
            name,
            Stdio {
                stdin: Some(stdin),
                stdout: Some(stdout),
                ..Stdio::null()
            },
        );
        running.push((program, hold, said));
    }
    let now = super::sys::inheritable_descriptors(me).expect("this process's descriptors");
    let made: Vec<(i32, u32)> = now.into_iter().filter(|d| !before.contains(d)).collect();
    for (mut program, hold, mut said) in running {
        drop(hold);
        let mut line = String::new();
        BufReader::new(&mut said)
            .read_line(&mut line)
            .expect("its line");
        assert_eq!(program.wait_for(HANG_GUARD).unwrap(), Some(Exit::Code(0)));
    }
    assert!(
        made.is_empty(),
        "descriptors (number, kind) a spawn that inherits would hand on: {made:?}"
    );
}

/// AC-T7.4.1 (CLAUDE.md §3; ROOT, 2026-11-13): a bound macOS can't hold of a process box is refused, typed, never made
/// with the bound dropped, as Windows refuses its own (`hex_core::job::limits`): a CPU cap, a memory level to throttle
/// at, and every disk bound. Do create a process box bounded at each, beside a memory bound it holds, through a
/// rootless runtime and through a box-user one; expect each refused as the platform's, naming its bound and macOS,
/// and no box made.
#[test]
fn a_process_box_bounded_where_macos_holds_no_bound_is_refused_typed() {
    let dir = tempfile::tempdir().expect("temp dir");
    let sock = dir.path().join("helper.sock");
    let listener = helper_daemon::bind(&sock).expect("bind the helper socket");
    let server = thread::spawn(move || {
        let daemon = Daemon::new(Sim::default(), vec![7000, 7001], 64);
        let _ = daemon.serve_listener(listener.as_fd());
    });
    let rate = NonZeroU64::new(256 << 30).expect("nonzero");
    let lateness =
        hex_core::watch::lateness_seed(&super::machine_model().expect("this Mac's model"));
    let rootless = MacRuntime::new(rate, rate, lateness).expect("a rootless runtime");
    let helped = MacRuntime::detect(rate, rate, lateness, &sock).expect("detect box-user mode");
    let held = Bounds {
        memory_max: Some(256 << 20),
        ..Bounds::default()
    };
    let unheld: [(&str, Bounds); 5] = [
        (
            "cpu_max_milli",
            Bounds {
                cpu_max_milli: Some(363),
                ..held.clone()
            },
        ),
        (
            "memory_high",
            Bounds {
                memory_high: Some(128 << 20),
                ..held.clone()
            },
        ),
        (
            "io_weight",
            Bounds {
                io_weight: Some(100),
                ..held.clone()
            },
        ),
        (
            "io_latency_us",
            Bounds {
                io_latency_us: Some(1_000),
                ..held.clone()
            },
        ),
        (
            "io_max",
            Bounds {
                io_max: Some(hex_api::IoRates {
                    read_bps: Some(1 << 20),
                    ..hex_api::IoRates::default()
                }),
                ..held.clone()
            },
        ),
    ];
    for rt in [&rootless, &helped] {
        for (bound, bounds) in &unheld {
            let id = BoxId::new("unheld").expect("box id");
            let refused = rt.create(BoxSpec {
                route: None,
                allocation: None,
                devices: Vec::new(),
                id: id.clone(),
                bounds: bounds.clone(),
                class: ClassSpec::Process,
                owner: None,
                band: 0,
            });
            assert!(
                matches!(
                    &refused,
                    Err(Refusal::Platform { what, platform: "macos", .. }) if what == bound
                ),
                "{bound}: {refused:?}"
            );
            assert!(matches!(rt.inspect(&id), Err(Refusal::NoSuchBox(_))));
        }
    }
    drop((rootless, helped));
    drop(server);
}

/// Joint plan step 14 (D260; AC-T7.4.1): a process's life runs from its start, as `proc_pid_rusage` reads it, to now on
/// the same clock. Do take this test process's start; note the time since this test began; read its life, then again;
/// then read the life of a start past every now. Expect its life at least the time the test had run (the process began
/// before it), no more than the machine has been up, never less on its second read; and the start past now refused,
/// typed.
#[test]
fn w14_d260_a_process_life_runs_from_its_start_to_now_on_one_clock() {
    let began = std::time::Instant::now();
    let pid = i32::try_from(std::process::id()).unwrap();
    let started = super::sys::rusage(pid).unwrap().unwrap().started;
    let ran = began.elapsed();
    let life = super::process_life(started).unwrap();
    assert!(life >= ran, "{life:?} against {ran:?}");
    assert!(life <= super::sys::uptime_raw().unwrap(), "{life:?}");
    assert!(super::process_life(started).unwrap() >= life);
    assert!(matches!(
        super::process_life(u64::MAX),
        Err(Refusal::Invalid {
            what: "a process's start",
            ..
        })
    ));
}

/// Joint plan step 14 (C1; ROOT, 2026-11-13; AC-T7.4.1): the managed control's cycle at a rate is its run and its hold at
/// that rate at the stop response its helper learned, the longer the lower the rate. Do take the cycle of a control whose
/// helper learned a 1 ms response at a thousandth of a CPU and at a CPU. Expect each its control's own run and hold at
/// that rate, the first the longer.
#[test]
fn w14_the_controls_cycle_at_a_rate_is_its_run_and_hold_there() {
    let ceiling = super::cpu_ceiling().unwrap();
    let response = Duration::from_millis(1);
    let estimate = Some(Estimate {
        response,
        cold: false,
    });
    let at = |rate| {
        let (run, hold) = super::cpu::cycle(rate, ceiling, response).unwrap();
        run + hold
    };
    assert_eq!(
        super::cycle_of(1, ceiling, estimate).unwrap(),
        crate::guest::CapCycle::Of(at(1))
    );
    assert_eq!(
        super::cycle_of(1_000, ceiling, estimate).unwrap(),
        crate::guest::CapCycle::Of(at(1_000))
    );
    assert!(at(1) > at(1_000));
}

/// The review's A-14 (ROOT, 2026-11-14; AC-T7.4.1): a cycle at a cold estimate of the control's stop response, the
/// slowest of too few holds to have seen their p99.9 (or an older helper's slowest seen), is used, and told cold. Do take
/// the cycle at a thousandth of a CPU at a cold 1 ms estimate and at a settled one. Expect both the control's run and
/// hold at 1 ms, the cold one saying why, the settled one plain.
#[test]
fn a14_a_cycle_at_a_cold_estimate_is_used_and_told_cold() {
    let ceiling = super::cpu_ceiling().unwrap();
    let response = Duration::from_millis(1);
    let (run, hold) = super::cpu::cycle(1, ceiling, response).unwrap();
    let cold = super::cycle_of(
        1,
        ceiling,
        Some(Estimate {
            response,
            cold: true,
        }),
    )
    .unwrap();
    assert!(
        matches!(
            &cold,
            crate::guest::CapCycle::ColdOf { cycle, why }
                if *cycle == run + hold && why.contains("slowest stop")
        ),
        "{cold:?}"
    );
    assert_eq!(
        super::cycle_of(
            1,
            ceiling,
            Some(Estimate {
                response,
                cold: false,
            }),
        )
        .unwrap(),
        crate::guest::CapCycle::Of(run + hold)
    );
}

/// The review's A-14 (2c-4b; ROOT, 2026-11-13: no record seeds, a cold start told): the node's control takes its cycle
/// from its helper's learned stop response, never a number its node configured, so before its helper has measured a
/// stop, or with no helper running, its cycle is cold, told. Do make a runtime whose control its node configured cold, as
/// it sends it, with no managed helper, and take its cycle at a thousandth of a CPU and at its ceiling. Expect the first
/// cold, naming its helper's measurement, and the second never.
#[test]
fn w14_2c4b_a_control_whose_helper_measured_no_stop_has_a_cold_cycle() {
    let model = super::machine_model().unwrap();
    let rates = hex_core::watch::seed(&model);
    let runtime = MacRuntime::new(
        rates.bytes,
        rates.processes,
        hex_core::watch::lateness_seed(&model),
    )
    .unwrap()
    .with_cpu_control(super::CpuControl::cold([7; 32]))
    .unwrap();
    let (_, ceiling) = runtime.cpu_control.unwrap();
    assert!(
        matches!(
            runtime.cap_cycle(1).unwrap(),
            crate::guest::CapCycle::Cold(why) if why.contains("managed helper")
        ),
        "a cycle cold until its helper measures a stop"
    );
    assert_eq!(
        runtime.cap_cycle(ceiling).unwrap(),
        crate::guest::CapCycle::Never
    );
}

/// The run's thread's stop at the hold on its box (the review's E-11): a fact no recorded operation makes, waited for
/// measuring, following the run's thread (`hex_testkit::waits::follow`).
static RUN_STOPPED: hex_core::patience::Patience =
    hex_core::patience::Patience::measuring("test.hex_runtime.run_stopped_at_hold");
/// A delete's wait on the word of the run's thread removing the box, or its answer beside it, following the delete.
static RUN_BESIDE: hex_core::patience::Patience =
    hex_core::patience::Patience::measuring("test.hex_runtime.delete_beside_run_removal");

/// A process box of the default bounds.
fn run_spec(id: &BoxId) -> BoxSpec {
    BoxSpec {
        route: None,
        allocation: None,
        devices: Vec::new(),
        id: id.clone(),
        bounds: Bounds::default(),
        class: ClassSpec::Process,
        owner: None,
        band: 0,
    }
}

/// A program that runs until it is ended or its stdin closes (the test's end among them): a fact, never a delay.
fn until_ended() -> Program {
    Program::of(&["/bin/cat"])
}

/// Its stdin a pipe the runtime makes, whose end the caller holds ([`Proc::streams`]).
fn fed() -> Stdio {
    Stdio {
        ask: hex_api::Ask {
            stdin: true,
            ..hex_api::Ask::default()
        },
        ..Stdio::null()
    }
}

/// A rootless runtime at this Mac's model's pacing.
fn rootless() -> MacRuntime {
    let model = super::machine_model().expect("this Mac's model");
    let rates = hex_core::watch::seed(&model);
    MacRuntime::new(
        rates.bytes,
        rates.processes,
        hex_core::watch::lateness_seed(&model),
    )
    .expect("a rootless runtime")
}

/// A box-user runtime whose helper's backend is [`Shells`] on a socket in `dir`, with two pool users.
fn helped(dir: &std::path::Path) -> MacRuntime {
    let sock = dir.join("helper.sock");
    let listener = helper_daemon::bind(&sock).expect("bind the helper socket");
    thread::spawn(move || {
        let daemon = Daemon::new(Shells::default(), vec![7000, 7001], 64);
        let _ = daemon.serve_listener(listener.as_fd());
    });
    MacRuntime::detect(
        NonZeroU64::new(1 << 40).unwrap(),
        NonZeroU64::new(1_000_000).unwrap(),
        hex_core::watch::lateness_seed(&super::machine_model().expect("this Mac's model")),
        &sock,
    )
    .expect("a box-user runtime")
}

/// Ends a box-user run's program as its helper would: [`Shells`]' own kill is a stand-in's no-op.
fn kill_by_pid(run: &mut dyn Proc) {
    let pid = rustix::process::Pid::from_raw(i32::try_from(run.pid()).unwrap()).unwrap();
    rustix::process::kill_process(pid, rustix::process::Signal::KILL).unwrap();
}

/// Ends a rootless run's program through its box's KILL.
fn kill_by_run(run: &mut dyn Proc) {
    run.kill().unwrap();
}

/// The review's E-11 (runtime goal 7, decision 7), AC-T7.4.1: a run's late removal never takes a later box of its id.
/// Do: hold run box `name`'s thread once its program has ended; run a program in it until it is ended, and end it
/// (`end`); once the run's thread has stopped at the hold, delete the box, make it again and start a program in it;
/// let the run's thread go and take its end. Expect: the run ends Signal(9); the later box stands and its program
/// runs on.
fn a_runs_late_removal_leaves_a_later_box(rt: &MacRuntime, name: &str, end: fn(&mut dyn Proc)) {
    let id = BoxId::new(name).unwrap();
    let held = crate::run::hold::hold(&id, crate::run::hold::At::End);
    let mut run = rt
        .run(run_spec(&id), until_ended(), fed())
        .expect("run a program until it is ended");
    let _feeding = run.streams().unwrap().stdin;
    end(&mut *run);
    hex_testkit::waits::follow::until(
        &RUN_STOPPED,
        "the run's thread at the hold, its program ended",
        || held.thread(),
        || held.stopped(),
    )
    .unwrap();
    rt.delete(&id).expect("delete the run's box");
    rt.create(run_spec(&id)).expect("make the box again");
    let mut later = rt
        .exec(&id, until_ended(), fed())
        .expect("start a program in the later box");
    let _feeding_later = later.streams().unwrap().stdin;
    drop(held);
    assert_eq!(
        hex_api::conformance::end_of(&mut *run),
        Ok(Exit::Signal(9)),
        "the run's end"
    );
    assert!(
        rt.inspect(&id).is_ok(),
        "the run's late removal took the later box of its id: {:?}",
        rt.inspect(&id)
    );
    assert_eq!(
        later.try_wait(),
        Ok(None),
        "the run's late removal ended the later box's program"
    );
    end(&mut *later);
    rt.delete(&id).expect("delete the later box");
}

/// The review's E-11 (runtime goal 7), AC-T7.4.1: a delete of a run's box that its program's end is removing answers as
/// that removal does, never beside it. Do: hold run box `name`'s thread once it has claimed the box's removal; run a
/// program in it until it is ended, and end it (`end`); once the run's thread has stopped at the hold, delete the box
/// from another thread; once that delete waits on the removal or answers, let the removal go. Expect: the delete waits
/// rather than answer while the removal is under way, then answers Ok; the run ends Signal(9); and the box is gone.
fn a_delete_beside_a_runs_removal_waits_for_its_word(
    rt: &MacRuntime,
    name: &str,
    end: fn(&mut dyn Proc),
) {
    let id = BoxId::new(name).unwrap();
    let held = crate::run::hold::hold(&id, crate::run::hold::At::Removal);
    let mut run = rt
        .run(run_spec(&id), until_ended(), fed())
        .expect("run a program until it is ended");
    let _feeding = run.streams().unwrap().stdin;
    end(&mut *run);
    hex_testkit::waits::follow::until(
        &RUN_STOPPED,
        "the run's thread at the hold, its removal claimed",
        || held.thread(),
        || held.stopped(),
    )
    .unwrap();
    thread::scope(|s| {
        let deleting = s.spawn(|| rt.delete(&id));
        hex_testkit::waits::follow::until(
            &RUN_BESIDE,
            "the delete's wait on the run's removal, or its answer",
            || {
                if deleting.is_finished() {
                    hex_core::patience::Party::Ended
                } else {
                    hex_core::patience::Party::Live
                }
            },
            || held.waiting() == 1 || deleting.is_finished(),
        )
        .unwrap();
        assert!(
            !deleting.is_finished(),
            "the delete answered beside the run's removal under way"
        );
        drop(held);
        assert_eq!(
            deleting.join().unwrap(),
            Ok(()),
            "the delete once the removal went"
        );
    });
    assert_eq!(hex_api::conformance::end_of(&mut *run), Ok(Exit::Signal(9)));
    assert!(matches!(rt.inspect(&id), Err(Refusal::NoSuchBox(_))));
}

/// A thread's handle as a party: live until it has finished.
fn party<T>(thread: &thread::ScopedJoinHandle<'_, T>) -> hex_core::patience::Party {
    if thread.is_finished() {
        hex_core::patience::Party::Ended
    } else {
        hex_core::patience::Party::Live
    }
}

/// The review's E-11 (runtime goal 7), AC-T7.4.1: a delete of a run's box while the run is making it waits for the box
/// to be made whole and its program watched, then takes it, as a delete on Linux waits on the run's create turn. Do:
/// hold run box `name`'s setup once its box is made and its program started, before the program's watch starts; run a
/// program in it until it is ended, from another thread; once the setup has stopped at the hold, delete the box from
/// another thread; once that delete waits on the setup or answers, let the setup go; then end the run's program
/// (`end`). Expect: the delete waits rather than answer while the box is being made, then answers Ok; the run ends as
/// its mode tells a program whose box a delete took (`ends`); and the box is gone.
fn a_delete_of_a_run_being_made_waits_for_its_making(
    rt: &MacRuntime,
    name: &str,
    end: fn(&mut dyn Proc),
    ends: Exit,
) {
    let id = BoxId::new(name).unwrap();
    let held = crate::run::hold::hold(&id, crate::run::hold::At::Making);
    thread::scope(|s| {
        let running = s.spawn(|| rt.run(run_spec(&id), until_ended(), fed()));
        hex_testkit::waits::follow::until(
            &RUN_STOPPED,
            "the run's setup at the hold, its box made",
            || party(&running),
            || held.stopped(),
        )
        .unwrap();
        let deleting = s.spawn(|| rt.delete(&id));
        hex_testkit::waits::follow::until(
            &RUN_BESIDE,
            "the delete's wait on the run's making, or its answer",
            || party(&deleting),
            || held.waiting() == 1 || deleting.is_finished(),
        )
        .unwrap();
        assert!(
            !deleting.is_finished(),
            "the delete answered beside the run's making"
        );
        drop(held);
        let mut run = running.join().unwrap().expect("the run, once made");
        let _feeding = run.streams().unwrap().stdin;
        assert_eq!(
            deleting.join().unwrap(),
            Ok(()),
            "the delete once the run was made"
        );
        end(&mut *run);
        assert_eq!(hex_api::conformance::end_of(&mut *run), Ok(ends));
    });
    assert!(matches!(rt.inspect(&id), Err(Refusal::NoSuchBox(_))));
}

/// E-11 in the rootless process-group mode (step 1).
#[test]
fn e11_a_rootless_runs_late_removal_leaves_a_later_box_of_its_id() {
    a_runs_late_removal_leaves_a_later_box(&rootless(), "e11-late-rootless", kill_by_run);
}

/// E-11 in the box-user mode (step 2), through a daemon on a real socket.
#[test]
fn e11_a_box_user_runs_late_removal_leaves_a_later_box_of_its_id() {
    let dir = tempfile::tempdir().expect("temp dir");
    a_runs_late_removal_leaves_a_later_box(&helped(dir.path()), "e11-late-helped", kill_by_pid);
}

/// E-11 in the rootless process-group mode (step 1).
#[test]
fn e11_a_rootless_delete_beside_a_runs_removal_waits_for_its_word() {
    a_delete_beside_a_runs_removal_waits_for_its_word(
        &rootless(),
        "e11-beside-rootless",
        kill_by_run,
    );
}

/// E-11 in the box-user mode (step 2), through a daemon on a real socket.
#[test]
fn e11_a_box_user_delete_beside_a_runs_removal_waits_for_its_word() {
    let dir = tempfile::tempdir().expect("temp dir");
    a_delete_beside_a_runs_removal_waits_for_its_word(
        &helped(dir.path()),
        "e11-beside-helped",
        kill_by_pid,
    );
}

/// E-11 in the rootless process-group mode (step 1): the delete's KILL ends the run's program, Signal(9).
#[test]
fn e11_a_rootless_delete_of_a_run_being_made_waits_for_its_making() {
    a_delete_of_a_run_being_made_waits_for_its_making(
        &rootless(),
        "e11-making-rootless",
        kill_by_run,
        Exit::Signal(9),
    );
}

/// E-11 in the box-user mode (step 2), through a daemon on a real socket: the helper keeps a dropped box's programs
/// until their own ends come, never an end made up at a time (the review's E-12), so the run's end, read after the
/// delete, is its KILL's, Signal(9), as in the rootless mode.
#[test]
fn e11_a_box_user_delete_of_a_run_being_made_waits_for_its_making() {
    let dir = tempfile::tempdir().expect("temp dir");
    a_delete_of_a_run_being_made_waits_for_its_making(
        &helped(dir.path()),
        "e11-making-helped",
        kill_by_pid,
        Exit::Signal(9),
    );
}

/// A delete's or a recovery's end's wait on a killed box's group past W*, stopped at the hold on its emptying, or its
/// answer: a fact no recorded operation makes, waited for measuring, following the ending thread.
static EMPTYING_FOLLOWED: hex_core::patience::Patience =
    hex_core::patience::Patience::measuring("test.hex_runtime.emptying_followed");

/// How a test ends a box's killed group: the box's delete, or a recovery's end of its group `(pgid, leader's start)`.
type EndGroup = fn(&MacRuntime, &BoxId, (i32, u64)) -> Result<(), Refusal>;

/// The review's E-12 (decision 9), AC-T7.4.1: a killed box's group that empties late is followed to its emptying, never
/// refused at a bound. Do: make rootless box `name` and start a program in it until it is ended; hold its group's
/// emptying, as a group whose last member's teardown runs long reads; end the group from another thread (`end`); once
/// that end follows the group past W* (stopped at the hold) or answers, let the hold go. Expect: the end hadn't answered
/// while the group read as not yet empty, then answers Ok; the program ends Signal(9).
fn a_late_emptying_is_followed_to_its_end(name: &str, end: EndGroup) {
    let rt = rootless();
    let id = BoxId::new(name).unwrap();
    rt.create(run_spec(&id)).expect("make the box");
    let mut program = rt
        .exec(&id, until_ended(), fed())
        .expect("start a program until it is ended");
    let _feeding = program.streams().unwrap().stdin;
    let group = rt
        .group(&id)
        .unwrap()
        .expect("the box's group, its program started");
    let held = crate::emptying::hold::hold(&group.0.to_string());
    thread::scope(|s| {
        let ending = s.spawn(|| end(&rt, &id, group));
        hex_testkit::waits::follow::until(
            &EMPTYING_FOLLOWED,
            "the end's wait following the group past W*, or its answer",
            || party(&ending),
            || held.following() || ending.is_finished(),
        )
        .unwrap();
        if ending.is_finished() {
            panic!(
                "the end answered while the group read as not yet empty: {:?}",
                ending.join()
            );
        }
        drop(held);
        assert_eq!(
            ending.join().unwrap(),
            Ok(()),
            "the end once the group emptied"
        );
    });
    assert_eq!(
        hex_api::conformance::end_of(&mut *program),
        Ok(Exit::Signal(9))
    );
    let cleanup = rt.delete(&id);
    assert!(
        matches!(cleanup, Ok(()) | Err(Refusal::NoSuchBox(_))),
        "{cleanup:?}"
    );
}

/// E-12 at a box's delete (`ProcessBox::delete`).
#[test]
fn e12_a_delete_follows_a_late_emptying_to_its_end() {
    a_late_emptying_is_followed_to_its_end("e12-delete", |rt, id, _| rt.delete(id));
}

/// E-12 at a recovery's end of a box's group (`MacRuntime::end`, `process::end_group`).
#[test]
fn e12_a_recoverys_end_follows_a_late_emptying_to_its_end() {
    a_late_emptying_is_followed_to_its_end("e12-recovery", |_, _, group| {
        MacRuntime::end(Some(group))
    });
}
