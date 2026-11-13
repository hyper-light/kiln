//! Exact ROOT005 native original/control port. Production snapshot/cleanup behavior is unchanged.
//! Kernel peer-PID is UNAVAILABLE here; the private fixture joins native Child receipts instead.
//! The original control models one explicit interleaving: the watcher has not run yet.
use super::*;
use std::io::{Read as _, Write as _};
use std::os::unix::net::{UnixListener, UnixStream};
use std::os::unix::process::CommandExt as _;
use std::path::Path;
use std::process::{Command, Stdio};

use hex_api::{BackingKey, BackingPhase, CapacityResources, Execution, Proc as _, RuntimeCost};
use hex_core::record::{NativeRecord, PoolRecord, RECORD_VERSION};
use shards_api::{Identity, ManagedProcess as _, TerminalScope};

use super::super::managed::{Controller, Scope};
use super::super::proc::{Prepared, Reaping};

// Same test hang guard and evidence as macos/tests.rs; not a runtime reaction allowance.
const GUARD: Duration = Duration::from_millis(39_507);
const WORKER: &str = "macos::process::scope_completion_tests::native_scope_worker";
const ROLE: &str = "HEX_SCOPE_CONTROL_ROLE";
const ENDPOINT: &str = "HEX_SCOPE_CONTROL_ENDPOINT";

fn command(role: &str, endpoint: &Path) -> Command {
    let mut command = Command::new(std::env::current_exe().unwrap());
    command
        .args(["--exact", WORKER, "--ignored", "--nocapture"])
        .env(ROLE, role)
        .env(ENDPOINT, endpoint)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::inherit());
    command
}

fn incoming(listener: &UnixListener, deadline: Instant) -> UnixStream {
    use rustix::event::{PollFd, PollFlags, Timespec, poll};
    loop {
        let left = deadline.saturating_duration_since(Instant::now());
        assert!(!left.is_zero(), "private worker connection deadline");
        let timeout = Timespec::try_from(left).unwrap();
        let mut descriptors = [PollFd::new(listener, PollFlags::IN)];
        match poll(&mut descriptors, Some(&timeout)) {
            Err(rustix::io::Errno::INTR) => continue,
            Ok(0) => panic!("private worker connection deadline"),
            Ok(_) => {}
            Err(error) => panic!("private worker connection readiness: {error}"),
        }
        match listener.accept() {
            Ok((stream, _)) => {
                stream.set_nonblocking(false).unwrap();
                stream
                    .set_read_timeout(Some(deadline.saturating_duration_since(Instant::now())))
                    .unwrap();
                stream
                    .set_write_timeout(Some(deadline.saturating_duration_since(Instant::now())))
                    .unwrap();
                return stream;
            }
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => continue,
            Err(error) => panic!("private worker accept: {error}"),
        }
    }
}

#[derive(Debug)]
pub(super) struct Birth {
    pub(super) pid: i32,
    pub(super) started: u64,
    pub(super) session: i32,
}

// This test-only owner retains the real Child plus its ONE native waiter. Installing a
// ProcessBox moves that waiter; it never creates a second waiter or calls Child::wait.
struct OwnedChild {
    child: std::process::Child,
    observer: Option<super::super::proc::BoxProc>,
    scope: Option<ProcessBox>,
    deadline: Instant,
    label: &'static str,
    reaped: bool,
    may_kill: bool,
}
impl OwnedChild {
    fn spawn(command: &mut Command, label: &'static str, deadline: Instant) -> Self {
        sys::retained_children().unwrap();
        let prepared = Prepared::new().unwrap(); // fallible preparation precedes spawn
        let mut owner = Self {
            child: command.spawn().unwrap(),
            observer: None,
            scope: None,
            deadline,
            label,
            reaped: false,
            may_kill: true,
        };
        eprintln!(
            "OWNED actor spawned: role={label} child_id={}",
            owner.child.id()
        );
        let pid = i32::try_from(owner.child.id()).unwrap();
        let (observer, registered) = prepared.bind(pid, None, Reaping::Retained);
        owner.observer = Some(observer);
        let birth = sys::rusage(pid);
        eprintln!(
            "OWNED actor native identity: role={label} pid={pid} birth={birth:?} registered={registered:?}"
        );
        registered.unwrap();
        owner
    }
    fn wait_exit(&mut self) -> Result<Option<hex_api::Exit>, String> {
        let left = self.deadline.saturating_duration_since(Instant::now());
        let result = if let Some(scope) = &self.scope {
            let mut held = scope
                .controller
                .lock()
                .map_err(|error| format!("{error}"))?;
            match &mut *held {
                Controller::Held { process, .. } => process.wait_for(left),
                _ => return Err("same controller waiter is unavailable".into()),
            }
        } else {
            self.observer
                .as_mut()
                .ok_or("owned native waiter is unavailable")?
                .wait_for(left)
        };
        if result.is_err() {
            self.may_kill = false;
        }
        result.map_err(|error| format!("{error:?}"))
    }
    fn reap(&mut self) -> Result<(), String> {
        let result = if let Some(scope) = &self.scope {
            let mut held = scope
                .controller
                .lock()
                .map_err(|error| format!("{error}"))?;
            match &mut *held {
                Controller::Held { process, .. } => process.finalize_retained(),
                _ => return Err("same controller waiter is unavailable".into()),
            }
        } else {
            self.observer
                .as_mut()
                .ok_or("owned native waiter is unavailable")?
                .finalize_retained()
        };
        match result {
            Ok(()) => {
                self.reaped = true;
                Ok(())
            }
            Err(error) => {
                self.may_kill = false;
                Err(format!("{error:?}"))
            }
        }
    }
    fn cleanup(&mut self, graceful: bool) -> Result<(), String> {
        if self.reaped {
            return Ok(());
        }
        let mut terminal = if graceful { self.wait_exit()? } else { None };
        if terminal.is_none() {
            if !self.may_kill {
                return Err("wait ownership uncertain; no PID signal permitted".into());
            }
            // It remains this owner's unreaped direct child. A failed/ambiguous wait disables
            // this path; a reaped/reused identity is never signaled by a later drop.
            let killed = self.child.kill();
            eprintln!(
                "OWNED actor cleanup signal: role={} pid={} result={killed:?}",
                self.label,
                self.child.id()
            );
            terminal = self.wait_exit()?;
        }
        if terminal.is_none() {
            return Err("original cleanup deadline expired; native cleanup UNKNOWN".into());
        }
        let reaped = self.reap();
        eprintln!(
            "OWNED actor cleanup: role={} pid={} terminal={terminal:?} reaped={reaped:?}",
            self.label,
            self.child.id()
        );
        reaped
    }
    fn install_scope(&mut self, birth: &Birth) {
        self.scope = Some(unobserved_box(birth));
        let process = self.observer.take().unwrap();
        *self.scope.as_ref().unwrap().controller.lock().unwrap() = Controller::Held {
            process,
            failure: None,
        };
    }
}
impl Drop for OwnedChild {
    fn drop(&mut self) {
        if !self.reaped {
            let result = self.cleanup(false);
            eprintln!(
                "OWNED actor drop cleanup: role={} pid={} result={result:?}",
                self.label,
                self.child.id()
            );
        }
    }
}

struct Escaped {
    stream: Option<UnixStream>,
    pid: i32,
    started: u64,
    ended: Kqueue,
}
struct Scenario {
    controller: OwnedChild,
    control: Option<UnixStream>,
    escaped: Option<Escaped>,
    child_requested: bool,
    cleaned: Option<bool>,
}
impl Scenario {
    fn cleanup(&mut self) -> bool {
        if let Some(complete) = self.cleaned {
            return complete;
        }
        // Before orphan permission, EOF makes the controller reap its own escrowed child.
        // After permission, the parent already owns the escaped peer's exact lifeline/watch.
        drop(self.control.take());
        let mut escaped_exit = !self.child_requested;
        if let Some(escaped) = &mut self.escaped {
            let release = escaped
                .stream
                .take()
                .map(|mut stream| stream.write_all(b"X"));
            let mut events = Vec::new();
            let waited = escaped.ended.wait(
                None,
                Some(
                    self.controller
                        .deadline
                        .saturating_duration_since(Instant::now()),
                ),
                &mut events,
            );
            escaped_exit = events
                .iter()
                .any(|event| matches!(event, sys::Event::Exit { pid, .. } if *pid == escaped.pid));
            eprintln!(
                "OWNED escaped cleanup request: pid={} birth={} lifeline={release:?} wait={waited:?} events={events:?}",
                escaped.pid, escaped.started
            );
        }
        let controller = self.controller.cleanup(true);
        let mut absent = true;
        if let Some(escaped) = &self.escaped {
            let after = sys::rusage(escaped.pid);
            absent = matches!(after, Ok(None))
                || matches!(after, Ok(Some(usage)) if usage.started != escaped.started);
            eprintln!(
                "OWNED escaped cleanup after controller reap: pid={} birth={} after={after:?} absence={absent}",
                escaped.pid, escaped.started
            );
        }
        let complete = controller.is_ok() && escaped_exit && absent;
        eprintln!(
            "CLEANUP scope facts: controller={controller:?} escaped_exit={escaped_exit} absence={absent} complete={complete}"
        );
        self.cleaned = Some(complete);
        complete
    }
}
impl Drop for Scenario {
    fn drop(&mut self) {
        if self.cleaned.is_none() {
            let _ = self.cleanup();
        }
    }
}

fn read_birth(stream: &mut UnixStream, role: u8, expected_pid: i32) -> Birth {
    let mut bytes = [0; 17];
    stream.read_exact(&mut bytes).unwrap();
    assert_eq!(bytes[0], role);
    let birth = Birth {
        pid: i32::from_be_bytes(bytes[1..5].try_into().unwrap()),
        started: u64::from_be_bytes(bytes[5..13].try_into().unwrap()),
        session: i32::from_be_bytes(bytes[13..17].try_into().unwrap()),
    };
    assert_eq!(
        sys::peer_euid(&*stream).unwrap(),
        rustix::process::geteuid().as_raw()
    );
    // ROOT005 has no kernel peer-PID wrapper. This TEST-ONLY private endpoint joins the
    // direct Child receipt (or its immutable controller's spawned Child receipt), not a
    // kernel peer-PID observation. Do not describe it as the stronger old cc2 fact.
    assert_eq!(birth.pid, expected_pid);
    assert!(birth.pid > 0 && birth.started > 0 && birth.session > 0);
    let actual = sys::rusage(birth.pid).unwrap().unwrap();
    assert_eq!(actual.started, birth.started);
    assert!(!actual.ended);
    birth
}

fn tell_birth(stream: &mut UnixStream, role: u8) {
    let pid = rustix::process::getpid().as_raw_nonzero().get();
    let actual = sys::rusage(pid).unwrap().unwrap();
    assert!(actual.started > 0 && !actual.ended);
    let session = rustix::process::getsid(None)
        .unwrap()
        .as_raw_nonzero()
        .get();
    let mut bytes = [0; 17];
    bytes[0] = role;
    bytes[1..5].copy_from_slice(&pid.to_be_bytes());
    bytes[5..13].copy_from_slice(&actual.started.to_be_bytes());
    bytes[13..17].copy_from_slice(&session.to_be_bytes());
    stream.write_all(&bytes).unwrap();
}

fn pool() -> PoolRecord {
    let fixed = CapacityResources {
        cpu_milli: 1000,
        mem_bytes: hex_api::MIN_MEMORY,
        disk_bytes: 0,
    };
    let cost = RuntimeCost {
        id: [3; 32],
        floor: fixed,
        ceiling: fixed,
        quantum: CapacityResources {
            cpu_milli: 1,
            mem_bytes: 1,
            disk_bytes: 1,
        },
        guest_overhead: CapacityResources::default(),
        overhead: CapacityResources::default(),
    };
    PoolRecord {
        version: RECORD_VERSION,
        key: BackingKey {
            pool: [5; 32],
            generation: [7; 16],
        },
        execution: Execution::ShardsShared,
        cost,
        allocation: Some(cost.allocate(CapacityResources::default()).unwrap()),
        devices: Vec::new(),
        phase: BackingPhase::Granted,
        init: None,
        exit: None,
    }
}

pub(super) fn unobserved_box(controller: &Birth) -> ProcessBox {
    let record = pool();
    record.check().unwrap();
    let native = NativeRecord::Pool(&record);
    // These are only existing watcher test model seeds. No watcher starts and this ROOT source
    // has no managed CPU control API; no enforcement/performance conclusion uses these values.
    let one = std::num::NonZeroU64::new(1).unwrap();
    let shared = Shared::new(one, one, Duration::from_nanos(1)).unwrap();
    // The record's own cost identity; the response is a test fixture, never a measured profile.
    let control = super::super::CpuControl {
        cost_id: record.cost.id,
        response: Duration::from_nanos(1),
    };
    let cpu = record.allocation.unwrap().resources.cpu_milli;
    let scope = Scope::new(
        native,
        Some((control, cpu)),
        Duration::ZERO,
        shared.cpu_start().unwrap(),
    )
    .unwrap();
    scope.held(native.owner()).unwrap();
    scope.running(native.owner()).unwrap();
    let bounds = Bounds::default();
    bounds.check().unwrap();
    let boxed = ProcessBox::prepared(
        native.owner().try_owned().unwrap(),
        &bounds,
        &shared,
        Some((controller.pid, controller.started)),
        Some(scope),
    )
    .unwrap();
    boxed
        .identity
        .set(Identity {
            pid: u32::try_from(controller.pid).unwrap(),
            started: controller.started,
            vsock: "/test-only-not-an-incarnation".into(),
        })
        .unwrap();
    assert!(boxed.watcher.lock().unwrap().is_none());
    boxed
}

/// AC-T7.4.1: an unobserved live descendant outside the PGID prevents whole-scope release.
/// This port runs exact ROOT005 production, independently of the old 2ef/858 original.
#[test]
#[ignore = "ROOT-only native original; spawns three exact owned test processes, no VM/GPU"]
fn unobserved_escaped_descendant_prevents_scope_release() {
    sys::retained_children().unwrap();
    let directory = tempfile::Builder::new()
        .prefix("hx-scope-")
        .tempdir()
        .unwrap();
    let endpoint = directory.path().join("s");
    let listener = UnixListener::bind(&endpoint).unwrap();
    listener.set_nonblocking(true).unwrap();
    let deadline = Instant::now().checked_add(GUARD).unwrap();
    let controller = OwnedChild::spawn(
        command("controller", &endpoint).process_group(0),
        "controller",
        deadline,
    );
    let mut scenario = Scenario {
        controller,
        control: None,
        escaped: None,
        child_requested: false,
        cleaned: None,
    };
    // A setup panic still passes through the same owned cleanup path before it is reported.
    let facts = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        scenario.control = Some(incoming(&listener, deadline));
        let controller_pid = i32::try_from(scenario.controller.child.id()).unwrap();
        let control = scenario.control.as_mut().unwrap();
        let controller_birth = sys::rusage(controller_pid).unwrap().unwrap();
        eprintln!("OWNED controller setup: child_id={controller_pid} native={controller_birth:?}");
        assert!(controller_birth.started > 0 && !controller_birth.ended);
        let controller = read_birth(control, b'C', controller_pid);
        assert_eq!(controller.started, controller_birth.started);
        assert_eq!(sys::group_of(controller.pid).unwrap(), Some(controller.pid));
        // No grandchild exists until this parent has joined the direct controller's birth.
        scenario.child_requested = true;
        control.write_all(b"S").unwrap();
        let mut child_identity = [0; 12];
        control.read_exact(&mut child_identity).unwrap();
        let child_pid =
            i32::try_from(u32::from_be_bytes(child_identity[..4].try_into().unwrap())).unwrap();
        let child_started = u64::from_be_bytes(child_identity[4..].try_into().unwrap());
        eprintln!("OWNED forwarded child identity: pid={child_pid} started={child_started}");
        assert!(child_pid > 0 && child_pid != controller.pid && child_started > 0);
        let child_birth = sys::rusage(child_pid).unwrap().unwrap();
        assert_eq!(child_birth.started, child_started);
        assert!(!child_birth.ended);
        let ended = Kqueue::new().unwrap();
        assert!(!ended.watch_end(child_pid).unwrap());
        scenario.escaped = Some(Escaped {
            stream: None,
            pid: child_pid,
            started: child_started,
            ended,
        });
        scenario.escaped.as_mut().unwrap().stream = Some(incoming(&listener, deadline));
        let escape = read_birth(
            scenario.escaped.as_mut().unwrap().stream.as_mut().unwrap(),
            b'E',
            child_pid,
        );
        assert_eq!(escape.started, child_started);
        assert_ne!(escape.session, controller.session);
        assert_eq!(sys::group_of(escape.pid).unwrap(), Some(escape.pid));
        assert_ne!(escape.pid, controller.pid);
        scenario.controller.install_scope(&controller);
        scenario.control.as_mut().unwrap().write_all(b"X").unwrap();
        let controller_exit = scenario.controller.wait_exit().unwrap();
        let boxed = scenario.controller.scope.as_ref().unwrap();
        let observation = boxed.terminal();
        let controller_fact = sys::rusage(controller.pid).unwrap();
        let escape_fact = sys::rusage(escape.pid).unwrap();
        let mut group = sys::listing().unwrap();
        let group_full = sys::group(controller.pid, &mut group).unwrap();
        let group_facts: Vec<_> = group
            .iter()
            .map(|pid| (*pid, sys::rusage(*pid).unwrap()))
            .collect();
        let escaped_count = boxed.status().unwrap().escaped;
        eprintln!(
            "ROOT005 ORIGINAL scope facts: kernel_peer_pid=UNAVAILABLE controller_child_id={} controller={controller:?} exit={controller_exit:?} native={controller_fact:?} group={group_facts:?} full={group_full} escaped={escape:?} native={escape_fact:?} observed={observation:?}",
            scenario.controller.child.id()
        );
        (
            controller,
            controller_exit,
            observation,
            controller_fact,
            escape_fact,
            group_full,
            group_facts,
            escaped_count,
        )
    }));
    let complete = scenario.cleanup();
    let (
        controller,
        controller_exit,
        observation,
        controller_fact,
        escape_fact,
        group_full,
        group_facts,
        escaped_count,
    ) = match facts {
        Ok(facts) => facts,
        Err(failure) => {
            eprintln!(
                "SETUP/OBSERVATION failed; cleanup_complete={complete}; no semantic red/green"
            );
            std::panic::resume_unwind(failure);
        }
    };
    assert!(
        complete,
        "INCOMPLETE cleanup; do not attribute this as a semantic red/green"
    );
    assert!(controller_exit.is_some());
    assert!(
        controller_fact.is_some_and(|usage| usage.started == controller.started && usage.ended)
    );
    assert!(escape_fact.is_some_and(|usage| usage.started
        == scenario.escaped.as_ref().unwrap().started
        && !usage.ended));
    assert!(!group_full);
    assert!(
        group_facts
            .iter()
            .all(|(_, usage)| usage.is_some_and(|usage| usage.ended))
    );
    assert_eq!(escaped_count, 0);
    assert!(
        !matches!(
            observation,
            Ok(Some(shards_api::ManagedEnd {
                scope: TerminalScope::Released,
                ..
            }))
        ),
        "live escaped birth was released from controller exit plus PGID observation"
    );
}

/// The helper roles have no VM/GPU/native-runtime effects. Only the parent above starts them.
#[test]
#[ignore = "internal ROOT native fixture role; requires exact parent-owned endpoint"]
fn native_scope_worker() {
    let deadline = Instant::now().checked_add(GUARD).unwrap();
    let role = std::env::var(ROLE).expect("internal fixture role");
    if role == "nproc" {
        nproc_control();
        return;
    }
    let endpoint = std::env::var_os(ENDPOINT).expect("internal fixture endpoint");
    if role == "escape" {
        rustix::process::setsid().unwrap();
    }
    let mut stream = UnixStream::connect(Path::new(&endpoint)).unwrap();
    stream.set_read_timeout(Some(GUARD)).unwrap();
    stream.set_write_timeout(Some(GUARD)).unwrap();
    match role.as_str() {
        "controller" => {
            tell_birth(&mut stream, b'C');
            let mut permission = [0];
            stream.read_exact(&mut permission).unwrap();
            assert_eq!(permission, *b"S");
            let child = OwnedChild::spawn(
                &mut command("escape", Path::new(&endpoint)),
                "escape",
                deadline,
            );
            let pid = i32::try_from(child.child.id()).unwrap();
            let birth = sys::rusage(pid).unwrap().unwrap();
            eprintln!("OWNED controller escrow: child_id={pid} native={birth:?}");
            assert!(birth.started > 0 && !birth.ended);
            stream.write_all(&child.child.id().to_be_bytes()).unwrap();
            stream.write_all(&birth.started.to_be_bytes()).unwrap();
            stream.read_exact(&mut permission).unwrap();
            assert_eq!(permission, *b"X");
            // The escaped child is intentionally live. Its parent's exit reassigns its reaping
            // to launchd; its exact private lifeline is already owned by the root fixture.
            // All earlier errors unwind the escrow and reap this owner's own child instead.
            // This exact test escrow survives its worker's natural libtest process exit. Its
            // Drop must not kill/reap the child after the parent accepted orphan ownership.
            let _escrow = std::mem::ManuallyDrop::new(child);
        }
        "escape" => {
            tell_birth(&mut stream, b'E');
            let mut command = [0];
            stream.read_exact(&mut command).unwrap();
            assert_eq!(command, *b"X");
        }
        other => panic!("unknown internal fixture role {other}"),
    }
}

fn nproc_control() {
    use rustix::process::{Resource, Rlimit, getrlimit, setrlimit};
    assert_ne!(
        rustix::process::getuid().as_raw(),
        0,
        "root bypasses XNU NPROC"
    );
    assert_ne!(
        rustix::process::geteuid().as_raw(),
        0,
        "root can raise hard limits"
    );
    let zero = Rlimit {
        current: Some(0),
        maximum: Some(0),
    };
    setrlimit(Resource::Nproc, zero).unwrap();
    assert_eq!(getrlimit(Resource::Nproc), zero);
    match Command::new(std::env::current_exe().unwrap())
        .arg("--list")
        .spawn()
    {
        Err(error) => assert_eq!(error.raw_os_error(), Some(libc::EAGAIN)),
        Ok(mut child) => {
            let mut waiter =
                super::super::proc::BoxProc::watch(i32::try_from(child.id()).unwrap(), None)
                    .unwrap();
            let _ = child.kill();
            assert!(waiter.wait_for(GUARD).unwrap().is_some());
            panic!("NPROC0 permitted a process creation");
        }
    }
    assert_eq!(
        setrlimit(
            Resource::Nproc,
            Rlimit {
                current: Some(1),
                maximum: Some(1)
            }
        ),
        Err(rustix::io::Errno::PERM)
    );
    assert_eq!(getrlimit(Resource::Nproc), zero);
    let child = std::thread::Builder::new().spawn(|| 13u64).unwrap();
    assert_eq!(child.join().unwrap(), 13);
}

/// Independent kernel primitive check; it does not certify HVF/Vulkan or pre-seal ownership.
#[test]
#[ignore = "ROOT-only native child: changes only its own irreversible hard NPROC limit"]
fn native_nproc_zero_refuses_spawn_and_hard_raise_but_preserves_threads() {
    let deadline = Instant::now().checked_add(GUARD).unwrap();
    let mut child = OwnedChild::spawn(
        &mut command("nproc", Path::new("unused")),
        "nproc",
        deadline,
    );
    let exit = child.wait_exit();
    let cleanup = child.cleanup(true);
    assert!(cleanup.is_ok(), "NPROC actor cleanup: {cleanup:?}");
    assert_eq!(exit.unwrap(), Some(hex_api::Exit::Code(0)));
}

/// Joint plan step 14 (2c; ROOT, 2026-11-13): a learning helper's scope takes its control's response from the helper's
/// own, never the launch's field, which is kept for older helpers: before any hold, its probe's. Do make a scope whose
/// launch names a 5 ms response on a helper that has seen a 2 us one, then one on a helper that has seen none. Expect
/// the first's burst its maximum over 2 us and the second's over the new helper's own start, neither over 5 ms.
#[test]
fn w14_a_learning_helpers_scope_reads_its_own_response_never_the_launchs() {
    let record = pool();
    let native = NativeRecord::Pool(&record);
    let control = super::super::CpuControl {
        cost_id: record.cost.id,
        response: Duration::from_millis(5),
    };
    let cpu = record.allocation.unwrap().resources.cpu_milli;
    let seen = Scope::new(
        native,
        Some((control, cpu)),
        Duration::ZERO,
        Duration::from_micros(2),
    )
    .unwrap();
    assert_eq!(seen.cpu_burst(), Some(i128::from(cpu) * 2_000));
    let one = std::num::NonZeroU64::new(1).unwrap();
    let shared = Shared::new(one, one, Duration::from_nanos(1)).unwrap();
    assert_eq!(shared.cpu_response().unwrap(), None);
    let start = shared.cpu_start().unwrap();
    assert_ne!(start, Duration::from_millis(5));
    let cold = Scope::new(native, Some((control, cpu)), Duration::ZERO, start).unwrap();
    assert_eq!(
        cold.cpu_burst(),
        Some(i128::from(cpu) * i128::try_from(start.as_nanos()).unwrap())
    );
}

/// Joint plan step 14 (2c; ROOT, 2026-11-13, cold start (a)): a cold helper probes its control's first stop response
/// before the first scope it controls starts from it: a spinner of its own held once at Hex's least CPU bound. Do ask a
/// new helper's start for a scope. Expect its slowest seen no longer none, a response of some time.
#[test]
fn w14_a_cold_helpers_first_scope_is_probed_for_its_response() {
    let one = std::num::NonZeroU64::new(1).unwrap();
    let shared = Shared::new(one, one, Duration::from_nanos(1)).unwrap();
    assert_eq!(shared.cpu_response().unwrap(), None);
    let start = shared.cpu_start().unwrap();
    assert_eq!(shared.cpu_response().unwrap(), Some(start));
    assert!(
        start > hex_core::helper::managed::COLD_RESPONSE,
        "{start:?}"
    );
}

/// Joint plan step 14 (2c; ROOT, 2026-11-13): a helper restarted with live scopes starts cold on purpose. Its slowest
/// stop lived in the old helper's memory, so a re-attached scope's control starts from the new helper's own, probed
/// before the scope is controlled, never the old helper's; and a first hold slower than that raises it, its overshoot
/// charged whole (`cpu::tests::a_held_scopes_slower_stop_tops_up_its_hold`). Do learn a 2 s response on one helper;
/// restart it (a new helper's shared state) and remake the live scope's control on it, as its re-attach does; then
/// learn a hold 1 ms slower than the new start. Expect the new helper's start its own probe's, not 2 s; the scope's
/// burst its maximum over that start, then over the hold's; and the new helper's slowest raised to the hold's.
#[test]
fn w14_a_restarted_helper_starts_its_live_scopes_cold_and_raises_on_their_first_hold() {
    let record = pool();
    let native = NativeRecord::Pool(&record);
    let control = super::super::CpuControl::cold(record.cost.id);
    let cpu = record.allocation.unwrap().resources.cpu_milli;
    let one = std::num::NonZeroU64::new(1).unwrap();
    let before = Shared::new(one, one, Duration::from_nanos(1)).unwrap();
    before.observe_cpu_response(Duration::from_secs(2)).unwrap();
    let after = Shared::new(one, one, Duration::from_nanos(1)).unwrap();
    let start = after.cpu_start().unwrap();
    assert!(
        start < Duration::from_secs(2),
        "the old helper's response carried: {start:?}"
    );
    let scope = Scope::new(native, Some((control, cpu)), Duration::ZERO, start).unwrap();
    let nanos = |d: Duration| i128::try_from(d.as_nanos()).unwrap();
    assert_eq!(scope.cpu_burst(), Some(i128::from(cpu) * nanos(start)));
    let hold = start + Duration::from_millis(1);
    scope.cpu_learn(native.owner(), hold).unwrap();
    after.observe_cpu_response(hold).unwrap();
    assert_eq!(scope.cpu_burst(), Some(i128::from(cpu) * nanos(hold)));
    assert_eq!(after.cpu_response().unwrap(), Some(hold));
}
