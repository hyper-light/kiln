//! ROOT-owned public borrowed-connector contract. No real VM is booted here.
//! Run only selected ignored cases with the coordinator's external process guard.
//!
//! Hex's own campaign over shards-api's public borrowed connector (`connect_borrowed`), the native Windows
//! steps among them (cold handles, WIN-NATIVE-STEPS): first carried as vendored Shards patches 0004, 0005, 0007
//! and 0029 (crates/api/tests/hex_borrowed_contract.rs), which Shards classed as Hex-only when it upstreamed the
//! rest (hex-upstream-v1's README at Shards 482c5d78), so it is a test of Hex's on its dependency. Its body is
//! unchanged (AC-T7.4.1).
// Hex's workspace lints, which Shards' crate didn't hold it to: its assertions and eprintln are Hex's
// disallowed macros, and its native observations (`started`, the Windows handle counts) are unsafe foreign
// calls, each with its SAFETY comment, which Hex's workspace denies by default.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    clippy::print_stderr,
    clippy::disallowed_macros,
    unsafe_code
)]

use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

#[cfg(windows)]
use shards_afunix::UnixListener;
use shards_api::{Identity, Refusal, connect_borrowed};
#[cfg(unix)]
use std::os::unix::net::UnixListener;

const PORT: u32 = 1234; // A protocol input, matching the canonical echo fixture.
const REQUEST: &[u8] = b"CONNECT 1234\n";

#[cfg(target_os = "linux")]
fn started(pid: u32, _child: Option<&Child>) -> u64 {
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).unwrap();
    stat.rsplit_once(')')
        .unwrap()
        .1
        .split_whitespace()
        .nth(19)
        .unwrap()
        .parse()
        .unwrap()
}

#[cfg(target_os = "macos")]
fn started(pid: u32, _child: Option<&Child>) -> u64 {
    // SAFETY: rusage_info_v0 is plain writable output for this exact libproc flavor.
    let mut usage: libc::rusage_info_v0 = unsafe { std::mem::zeroed() };
    assert_eq!(
        // SAFETY: pid names this fixture's current process or its owned child, and usage matches V0.
        unsafe {
            libc::proc_pid_rusage(
                i32::try_from(pid).unwrap(),
                libc::RUSAGE_INFO_V0,
                (&raw mut usage).cast(),
            )
        },
        0
    );
    usage.ri_proc_start_abstime
}

#[cfg(windows)]
fn started(_pid: u32, child: Option<&Child>) -> u64 {
    use std::os::windows::io::AsRawHandle;
    use windows_sys::Win32::Foundation::FILETIME;
    use windows_sys::Win32::System::Threading::{GetCurrentProcess, GetProcessTimes};
    let handle = match child {
        Some(child) => child.as_raw_handle(),
        None => {
            // SAFETY: this process's pseudo-handle is valid and remains borrowed.
            unsafe { GetCurrentProcess() }
        }
    };
    let mut created = FILETIME::default();
    let mut ended = FILETIME::default();
    let mut kernel = FILETIME::default();
    let mut user = FILETIME::default();
    assert_ne!(
        // SAFETY: handle is query-capable and all four outputs are distinct writable FILETIMEs.
        unsafe { GetProcessTimes(handle, &mut created, &mut ended, &mut kernel, &mut user) },
        0
    );
    (u64::from(created.dwHighDateTime) << u32::BITS) | u64::from(created.dwLowDateTime)
}

fn own_identity(path: PathBuf) -> Identity {
    let pid = std::process::id();
    Identity {
        pid,
        started: started(pid, None),
        vsock: path,
    }
}

fn echo_once(listener: &UnixListener) {
    let (mut peer, _) = listener.accept().unwrap();
    let mut request = [0; REQUEST.len()];
    peer.read_exact(&mut request).unwrap();
    assert_eq!(request, REQUEST);
    peer.write_all(b"OK 7\n").unwrap();
    assert_eq!(
        peer.read(&mut [0; 1]).unwrap(),
        0,
        "all borrowed stream clones must close"
    );
}

struct Target(Child);

impl Target {
    fn new() -> Self {
        Self(
            Command::new(std::env::current_exe().unwrap())
                .args([
                    "--ignored",
                    "--exact",
                    "root_borrowed_target_waits_for_stdin",
                    "--test-threads=1",
                ])
                .env("HEX_BORROWED_TARGET", "1")
                .stdin(Stdio::piped())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .spawn()
                .unwrap(),
        )
    }

    fn identity(&self, path: &Path) -> Identity {
        let pid = self.0.id();
        Identity {
            pid,
            started: started(pid, Some(&self.0)),
            vsock: path.to_owned(),
        }
    }
}

impl Drop for Target {
    fn drop(&mut self) {
        self.0.stdin.take();
        if self.0.try_wait().unwrap().is_none() {
            let _ = self.0.kill(); // Only this fixture's recorded owned child.
        }
        let _ = self.0.wait();
    }
}

#[test]
#[ignore = "ROOT child fixture only; requires HEX_BORROWED_TARGET=1 and an external guard"]
fn root_borrowed_target_waits_for_stdin() {
    assert_eq!(std::env::var("HEX_BORROWED_TARGET").as_deref(), Ok("1"));
    assert_eq!(std::io::stdin().read(&mut [0; 1]).unwrap(), 0);
}

#[test]
#[ignore = "ROOT process/socket lifetime contract; externally guard the entire invocation"]
fn root_borrowed_drop_preserves_target_and_socket_and_dead_target_refuses() {
    let dir = shards_testdir::TempDir::new("hb-life").unwrap();
    let path = dir.join("v");
    let listener = UnixListener::bind(&path).unwrap();
    let mut target = Target::new();
    let identity = target.identity(&path);
    let server = std::thread::spawn(move || {
        echo_once(&listener);
        echo_once(&listener);
    });
    for _ in 0..2 {
        // Drop, then reconnect to the same still-owned target/socket.
        let stream =
            connect_borrowed(&identity, PORT, shards_api::conformance::UNSERVED_PATIENCE).unwrap();
        let clone = stream.try_clone().unwrap();
        drop(stream);
        assert!(target.0.try_wait().unwrap().is_none());
        drop(clone);
        assert!(target.0.try_wait().unwrap().is_none());
        assert!(path.exists() && dir.path().is_dir());
    }
    server.join().unwrap();
    target.0.stdin.take();
    assert!(target.0.wait().unwrap().success());
    assert!(
        matches!(connect_borrowed(&identity, PORT, shards_api::conformance::UNSERVED_PATIENCE), Err(Refusal::Process { pid, expected_started, .. }) if pid == identity.pid && expected_started == identity.started)
    );
    assert!(path.exists() && dir.path().is_dir());
    eprintln!(
        "borrowed_lifetime live_after_both_drops=true reconnects=2 dead_target_process_refusal=true socket_directory_preserved=true"
    );
}

#[test]
#[ignore = "ROOT finite overflow/no-dial contract; externally guard the entire invocation"]
fn root_borrowed_unrepresentable_patience_refuses_before_dial() {
    let dir = shards_testdir::TempDir::new("hb-bound").unwrap();
    let path = dir.join("v");
    let listener = UnixListener::bind(&path).unwrap();
    listener.set_nonblocking(true).unwrap();
    let live = own_identity(path);
    let invalid = Identity {
        pid: 0,
        started: 0,
        ..live.clone()
    };
    let finite_overflow = Duration::MAX.checked_sub(Duration::from_nanos(1)).unwrap();
    assert_ne!(finite_overflow, Duration::MAX);
    assert!(Instant::now().checked_add(finite_overflow).is_none());
    for id in [&live, &invalid] {
        for patience in [Duration::MAX, finite_overflow] {
            assert!(matches!(
                connect_borrowed(id, PORT, patience),
                Err(Refusal::Bounds {
                    what: "patience",
                    ..
                })
            ));
            assert_eq!(
                listener.accept().unwrap_err().kind(),
                std::io::ErrorKind::WouldBlock
            );
        }
    }
    eprintln!(
        "borrowed_bounds max_and_finite_overflow=true live_and_invalid_identity=true accepted_connections=0"
    );
}

#[test]
#[ignore = "ROOT target death during CONNECT/OK; externally guard the entire invocation"]
fn root_borrowed_target_death_before_ok_is_refused() {
    let dir = shards_testdir::TempDir::new("hb-death").unwrap();
    let path = dir.join("v");
    let listener = UnixListener::bind(&path).unwrap();
    let mut target = Target::new();
    let identity = target.identity(&path);
    let server = std::thread::spawn(move || {
        let (mut peer, _) = listener.accept().unwrap();
        let mut request = [0; REQUEST.len()];
        peer.read_exact(&mut request).unwrap();
        assert_eq!(request, REQUEST);
        target.0.stdin.take();
        assert!(target.0.wait().unwrap().success());
        peer.write_all(b"OK 7\n").unwrap();
    });
    let result = connect_borrowed(&identity, PORT, shards_api::conformance::UNSERVED_PATIENCE);
    server.join().unwrap();
    assert!(
        matches!(result, Err(Refusal::Process { pid, expected_started, failure: shards_api::ProcessFailure::Exited }) if pid == identity.pid && expected_started == identity.started)
    );
    assert!(path.exists() && dir.path().is_dir());
    eprintln!(
        "borrowed_post_handshake target_reaped_before_ok=true exact_exited_refusal=true socket_directory_preserved=true"
    );
}

#[cfg(windows)]
fn handles() -> u32 {
    use windows_sys::Win32::System::Threading::{GetCurrentProcess, GetProcessHandleCount};
    let mut count = 0;
    assert_ne!(
        // SAFETY: a borrowed current-process pseudo-handle, valid writable DWORD, no handle acquired.
        unsafe { GetProcessHandleCount(GetCurrentProcess(), &mut count) },
        0
    );
    count
}

#[cfg(windows)]
fn stage_handles_requested() -> bool {
    match std::env::var("HEX_BORROWED_STAGE_HANDLES") {
        Err(std::env::VarError::NotPresent) => false,
        Ok(value) if value == "1" => true,
        _ => panic!("HEX_BORROWED_STAGE_HANDLES requires 1 or absence"),
    }
}

#[cfg(windows)]
fn cold_handle_outcome(before: u32, after: u32) -> &'static str {
    if before == after {
        "equal_observed"
    } else {
        "uncertain_delta"
    }
}

// Fixed test observations at the 19 explicit boundaries below, never a caller-selected buffer.
#[cfg(windows)]
type HandleStages = [(&'static str, u32); 19];

#[cfg(windows)]
fn one_handle_round(dir: &Path, bit: u32, trace: bool) -> Option<HandleStages> {
    let snapshot = || trace.then(handles);
    let before_round = snapshot();
    let path = dir.join("v");
    let id = own_identity(path.clone());
    let after_identity = snapshot();
    let mut wrong = id.clone();
    wrong.started ^= 1_u64 << bit;
    assert!(matches!(
        connect_borrowed(&wrong, PORT, Duration::ZERO),
        Err(Refusal::Process { .. })
    ));
    let after_wrong_start = snapshot();
    let missing = Identity {
        vsock: dir.join("missing"),
        ..id.clone()
    };
    assert!(matches!(
        connect_borrowed(&missing, PORT, shards_api::conformance::UNSERVED_PATIENCE),
        Err(Refusal::Borrowed { .. })
    ));
    let after_missing_path = snapshot();
    let invalid = Identity {
        pid: 0,
        started: 0,
        ..id.clone()
    };
    assert!(matches!(
        connect_borrowed(&invalid, PORT, Duration::MAX),
        Err(Refusal::Bounds {
            what: "patience",
            ..
        })
    ));
    let after_overflow = snapshot();

    let listener = UnixListener::bind(&path).unwrap();
    listener.set_nonblocking(true).unwrap();
    let after_zero_bind = snapshot();
    // No peer accepts or answers before the zero-patience call returns.
    let refused = connect_borrowed(&id, PORT, Duration::ZERO);
    let after_zero_call = snapshot();
    assert!(
        matches!(
            &refused,
            Err(Refusal::Borrowed {
                failure: shards_api::BorrowFailure::Channel(shards_api::HandshakeError::Io {
                    phase: shards_api::HandshakePhase::Dial
                        | shards_api::HandshakePhase::Write
                        | shards_api::HandshakePhase::Read,
                    kind: std::io::ErrorKind::WouldBlock | std::io::ErrorKind::Interrupted,
                    ..
                }),
                ..
            })
        ),
        "zero withheld reply: {:?}",
        refused.err()
    );
    let Err(Refusal::Borrowed {
        failure: shards_api::BorrowFailure::Channel(shards_api::HandshakeError::Io { phase, .. }),
        ..
    }) = refused
    else {
        panic!("expected typed readiness refusal")
    };
    eprintln!(
        "borrowed_zero_ready phase={phase:?} read_phase_observed={}",
        phase == shards_api::HandshakePhase::Read
    );
    let after_zero_report = snapshot();
    match listener.accept() {
        Ok((mut peer, _)) => {
            peer.set_nonblocking(true).unwrap();
            let mut bytes = [0; REQUEST.len()];
            match peer.read(&mut bytes) {
                Ok(n) => assert!(REQUEST.starts_with(&bytes[..n])),
                Err(error) => assert!(matches!(
                    error.kind(),
                    std::io::ErrorKind::WouldBlock | std::io::ErrorKind::ConnectionReset
                )),
            }
        }
        Err(error) => assert_eq!(error.kind(), std::io::ErrorKind::WouldBlock),
    }
    let after_zero_peer = snapshot();
    assert_eq!(
        listener.accept().unwrap_err().kind(),
        std::io::ErrorKind::WouldBlock,
        "zero patience must not retry"
    );
    drop(listener);
    let after_zero_listener_drop = snapshot();
    std::fs::remove_file(&path).unwrap();
    let after_zero_path_remove = snapshot();

    let listener = UnixListener::bind(&path).unwrap();
    let after_success_bind = snapshot();
    let server = std::thread::spawn(move || echo_once(&listener));
    let after_thread_spawn = snapshot();
    let stream = connect_borrowed(&id, PORT, shards_api::conformance::UNSERVED_PATIENCE).unwrap();
    let after_connect = snapshot();
    let clone = stream.try_clone().unwrap();
    let after_clone = snapshot();
    drop(stream);
    let after_original_drop = snapshot();
    drop(clone);
    let after_clone_drop = snapshot();
    server.join().unwrap();
    let after_thread_join = snapshot();
    std::fs::remove_file(&path).unwrap();
    let after_path_remove = snapshot();
    if !trace {
        return None;
    }
    Some([
        ("before_round", before_round.unwrap()),
        ("after_identity", after_identity.unwrap()),
        ("after_wrong_start", after_wrong_start.unwrap()),
        ("after_missing_path", after_missing_path.unwrap()),
        ("after_overflow", after_overflow.unwrap()),
        ("after_zero_bind", after_zero_bind.unwrap()),
        ("after_zero_call", after_zero_call.unwrap()),
        ("after_zero_report", after_zero_report.unwrap()),
        ("after_zero_peer", after_zero_peer.unwrap()),
        (
            "after_zero_listener_drop",
            after_zero_listener_drop.unwrap(),
        ),
        ("after_zero_path_remove", after_zero_path_remove.unwrap()),
        ("after_success_bind", after_success_bind.unwrap()),
        ("after_thread_spawn", after_thread_spawn.unwrap()),
        ("after_connect", after_connect.unwrap()),
        ("after_clone", after_clone.unwrap()),
        ("after_original_drop", after_original_drop.unwrap()),
        ("after_clone_drop", after_clone_drop.unwrap()),
        ("after_thread_join", after_thread_join.unwrap()),
        ("after_path_remove", after_path_remove.unwrap()),
    ])
}

#[cfg(windows)]
#[test]
#[ignore = "ROOT repeated native handle contract; required rounds and external process guard"]
fn root_borrowed_failures_and_drops_return_windows_handles() {
    let rounds: u32 = std::env::var("HEX_BORROWED_CASE_ROUNDS")
        .expect("ROOT supplies workload rounds")
        .parse()
        .unwrap();
    assert!(
        rounds > 0 && rounds <= u64::BITS,
        "one wrong-start bit per round"
    );
    let dir = shards_testdir::TempDir::new("hb-handles").unwrap();
    let trace = stage_handles_requested();
    let cold_before = handles();
    let stages = one_handle_round(dir.path(), 0, trace);
    let before = handles();
    eprintln!(
        "borrowed_windows_cold_handles before={cold_before} after={before} equal={} outcome={}",
        cold_before == before,
        cold_handle_outcome(cold_before, before)
    );
    // Report after the cold cut, so stage formatting does not become a measured first-use action.
    if let Some(stages) = stages {
        for (stage, count) in stages {
            eprintln!(
                "borrowed_windows_first_round_stage pid={} stage={stage} handles={count}",
                std::process::id()
            );
        }
    }
    for bit in 0..rounds {
        let _ = one_handle_round(dir.path(), bit, false);
        assert_eq!(
            handles(),
            before,
            "native handle count after complete round {bit}"
        );
    }
    eprintln!(
        "borrowed_windows_handles rounds={rounds} before={before} after={} all_rounds_equal=true",
        handles()
    );
}

/// HEX-RT-17: ROOT invokes this exact test alone in a fresh process with a short private TEMP.
#[cfg(windows)]
#[test]
#[ignore = "ROOT fresh-process AF_UNIX first-use control; requires stage opt-in and external guard"]
fn root_borrowed_cold_afunix_bind_drop_control() {
    assert!(
        stage_handles_requested(),
        "ROOT explicitly opts in to handle observations"
    );
    let before_fixture = handles();
    let dir = shards_testdir::TempDir::new("hb-bind").unwrap();
    let path = dir.join("v");
    let before_bind = handles();
    let listener = UnixListener::bind(&path).unwrap();
    let after_bind = handles();
    drop(listener);
    let after_drop = handles();
    std::fs::remove_file(&path).unwrap();
    drop(dir);
    let after_fixture = handles();
    eprintln!(
        "borrowed_windows_cold_control pid={} control=afunix_bind_drop before_fixture={before_fixture} before_bind={before_bind} after_bind={after_bind} after_drop={after_drop} after_fixture={after_fixture} outcome={}",
        std::process::id(),
        cold_handle_outcome(before_fixture, after_fixture)
    );
}

/// HEX-RT-17: ROOT invokes this exact test alone in a different fresh process; no socket is made.
#[cfg(windows)]
#[test]
#[ignore = "ROOT fresh-process thread first-use control; requires stage opt-in and external guard"]
fn root_borrowed_cold_thread_start_join_control() {
    assert!(
        stage_handles_requested(),
        "ROOT explicitly opts in to handle observations"
    );
    let before = handles();
    let thread = std::thread::Builder::new().spawn(|| ()).unwrap();
    let after_start = handles();
    thread.join().unwrap();
    let after_join = handles();
    eprintln!(
        "borrowed_windows_cold_control pid={} control=thread_start_join before={before} after_start={after_start} after_join={after_join} outcome={}",
        std::process::id(),
        cold_handle_outcome(before, after_join)
    );
}
