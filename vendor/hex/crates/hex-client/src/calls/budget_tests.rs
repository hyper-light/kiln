//! AC-T7.4.1: caller caps at the actual Core/guest setup seam, no VMM or GPU.
//!
//! Each wait here for something that should happen takes its own fact's patience (hex_testkit::waits; the lead's
//! rules of 2026-11-06 and 2026-11-08), never a picked window: no recorded operation makes any of these facts, so each
//! measures, for its fact alone, named `test.hex_client.budget.<fact>`, its answers recorded for the census that will
//! seed it. A capped call ends on its own cap however late its answer reaches the test. A window stays only where it is
//! the claim under test, and says so.

use super::*;
use crate::guest::{self, Duplex, DuplexOwner, Link, own_duplex};
use hex_testkit::waits::within;
use std::io::{self, Read, Write};
use std::net::{Ipv4Addr, Shutdown, SocketAddr, TcpListener, TcpStream, UdpSocket};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::mpsc;

// These are caller-supplied fixture policies, not production calibration or performance targets, and never waits.
const CAP: Duration = Duration::from_millis(25);
/// A caller bound longer than [`CAP`], for the claims that need one: a learned wait seeded from it ends after CAP, a
/// setup under it has room for its steps before CAP is left, and it is a later bound than CAP's.
const LONG_CAP: Duration = Duration::from_millis(100);

/// The next message from `from` by `by`, or with no deadline whenever it comes; a sender gone is `Disconnected`.
fn recv_by<T>(from: &mpsc::Receiver<T>, by: Option<Instant>) -> Result<T, mpsc::RecvTimeoutError> {
    match by {
        Some(by) => from.recv_timeout(by.saturating_duration_since(Instant::now())),
        None => from
            .recv()
            .map_err(|mpsc::RecvError| mpsc::RecvTimeoutError::Disconnected),
    }
}

/// Waits on `wait`, its fact's own patience, for the next message from `from`.
fn recv_on<T>(wait: &Patience, from: &mpsc::Receiver<T>) -> Result<T, mpsc::RecvTimeoutError> {
    within(wait, |by| recv_by(from, by))
}

/// A socket's timeout for a wait by `by`: none with no deadline, and a timed-out refusal once it has passed, a zero
/// timeout being none to a socket.
fn timeout_by(by: Option<Instant>) -> io::Result<Option<Duration>> {
    let Some(by) = by else {
        return Ok(None);
    };
    let left = by.saturating_duration_since(Instant::now());
    if left.is_zero() {
        return Err(io::ErrorKind::TimedOut.into());
    }
    Ok(Some(left))
}

/// The deadline a wait by `by` gives a call that must be given one: `by`, or with none the longest one wait on a
/// connection from now ([`MOST_ONE_WAIT`]), which no answer here comes near, so the call waits for its fact alone.
fn answer_by(by: Option<Instant>) -> Instant {
    by.unwrap_or_else(|| Instant::now().checked_add(MOST_ONE_WAIT).unwrap())
}

struct Stalled {
    socket: UdpSocket,
    sent: Mutex<Option<mpsc::SyncSender<()>>>,
    closed: AtomicBool,
    budget: Option<Duration>,
}

fn stalled(budget: Option<Duration>) -> (Stalled, mpsc::Receiver<()>) {
    let socket = UdpSocket::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
    socket.connect(socket.local_addr().unwrap()).unwrap();
    socket.set_nonblocking(true).unwrap();
    let (sent, request) = mpsc::sync_channel(1);
    (
        Stalled {
            socket,
            sent: Mutex::new(Some(sent)),
            closed: AtomicBool::new(false),
            budget,
        },
        request,
    )
}

impl Transport for Stalled {
    type Reading = ();
    fn answers() -> &'static [Patience; KINDS] {
        static WAITS: [Patience; KINDS] =
            [const { Patience::measuring("test.client_budget.unarmed") }; KINDS];
        &WAITS
    }
    fn reply_budget(&self) -> Option<Duration> {
        self.budget
    }
    fn reading(&self) {}
    fn handle(&self) -> Handle<'_> {
        #[cfg(unix)]
        return std::os::fd::AsFd::as_fd(&self.socket);
        #[cfg(windows)]
        std::os::windows::io::AsSocket::as_socket(&self.socket)
    }
    fn recv(&self, _: &mut (), _: &mut Take<'_>) -> Result<bool, Refusal> {
        let _ = self.socket.recv(&mut [0]);
        if self.closed.load(Ordering::Acquire) {
            Err(Refusal::Broken("owned original-control release".into()))
        } else {
            Ok(false)
        }
    }
    fn try_send(&self, bytes: &[u8], at: &mut usize, _: &[Lent<'_>]) -> Result<bool, Sent> {
        *at = bytes.len();
        if let Some(sent) = self.sent.lock().unwrap().take() {
            let _ = sent.send(());
        }
        Ok(true)
    }
    fn shutdown(&self) {
        self.closed.store(true, Ordering::Release);
        let _ = self.socket.send(&[1]);
    }
}

/// Do send an actual Core hello with an explicit caller cap and an unarmed learned wait;
/// expect caller expiry instead of waiting until the external original-control release.
#[test]
fn explicit_budget_caps_an_unarmed_core_hello() {
    /// The hello's refusal at its caller's cap.
    static CAPPED: Patience = Patience::measuring("test.hex_client.budget.unarmed_hello_capped");
    let socket = UdpSocket::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
    socket.connect(socket.local_addr().unwrap()).unwrap();
    socket.set_nonblocking(true).unwrap();
    let (sent, request) = mpsc::sync_channel(1);
    let shared = Shared::try_new(Stalled {
        socket,
        sent: Mutex::new(Some(sent)),
        closed: AtomicBool::new(false),
        budget: Some(CAP),
    })
    .unwrap();
    let control = triomphe::Arc::clone(&shared);
    let (finished, answer) = mpsc::sync_channel(1);
    let worker = std::thread::spawn(move || {
        let result = Core::hello(control).map(drop);
        finished.send(result).unwrap();
    });
    // Its send precedes its answer, so it is read once the answer has come: a hello whose cap passed before it sent
    // leaves the test nothing to wait for.
    let result = recv_on(&CAPPED, &answer);
    let observed_send = request.try_recv();
    // Always release and join the exact owned actor before asserting the original symptom.
    shared.end(Refusal::Broken("owned original-control release".into()));
    worker.join().unwrap();
    observed_send.unwrap();
    eprintln!(
        "client_budget_control phase=unarmed-hello cap={CAP:?} returned={}",
        result.is_ok()
    );
    assert!(
        result.is_ok(),
        "unarmed hello ignored the configured caller cap"
    );
    assert!(matches!(result.unwrap(), Err(Refusal::Os { .. })));
}

struct TokenLink {
    address: SocketAddr,
}
impl Link for TokenLink {
    fn reply_budget(&self) -> Option<Duration> {
        Some(CAP)
    }
    fn dial(&self, _: u32) -> Result<DuplexOwner, Refusal> {
        TcpStream::connect_timeout(&self.address, CAP)
            .map_err(|error| Refusal::os("bounded original-control dial", error))
            .and_then(own_duplex)
    }
    fn dial_before(&self, port: u32, deadline: Option<Instant>) -> Result<DuplexOwner, Refusal> {
        let Some(deadline) = deadline else {
            return self.dial(port);
        };
        let left = deadline.saturating_duration_since(Instant::now());
        if left.is_zero() {
            return Err(Refusal::os(
                "bounded original-control dial",
                std::io::ErrorKind::TimedOut,
            ));
        }
        TcpStream::connect_timeout(&self.address, left)
            .map_err(|error| Refusal::os("bounded original-control dial", error))
            .and_then(own_duplex)
    }
}

/// Do complete a bounded real loopback dial, then withhold the token;
/// expect the same caller cap to bound setup, rather than the peer's eventual close.
#[test]
fn explicit_budget_caps_token_after_a_bounded_dial() {
    /// The connect's dial, accepted.
    static DIALED: Patience = Patience::measuring("test.hex_client.budget.token_dialed");
    /// The connect's refusal at its caller's cap, the token withheld.
    static CAPPED: Patience = Patience::measuring("test.hex_client.budget.token_capped");
    let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
    listener.set_nonblocking(true).unwrap();
    let address = listener.local_addr().unwrap();
    let stop = AtomicBool::new(false);
    let wake = Wake::new().unwrap();
    let (finished, answer) = mpsc::sync_channel(1);
    let worker = std::thread::spawn(move || {
        finished
            .send(
                guest::own_link(TokenLink { address })
                    .and_then(guest::Client::connect)
                    .map(drop),
            )
            .unwrap();
    });
    let peer = within(&DIALED, |by| accept_before(&listener, &stop, &wake, by));
    let result = recv_on(&CAPPED, &answer);
    // No VM, signal or foreign PID: close only this test's peer and join its worker.
    let cleanup = peer.as_ref().map(|peer| peer.shutdown(Shutdown::Both));
    worker.join().unwrap();
    assert!(
        peer.is_ok(),
        "the token control must actually complete its dial"
    );
    assert!(
        matches!(cleanup, Ok(Ok(())))
            || matches!(cleanup, Ok(Err(error)) if error.kind() == std::io::ErrorKind::NotConnected)
    );
    eprintln!(
        "client_budget_control phase=token cap={CAP:?} returned={}",
        result.is_ok()
    );
    assert!(
        result.is_ok(),
        "token read outlived the configured bounded dial budget"
    );
    assert!(matches!(result.unwrap(), Err(Refusal::Os { .. })));
}

fn configured<T: Transport>(transport: T) -> Core<T> {
    let client = Core {
        conn: triomphe::Arc::try_new(Conn {
            shared: Shared::try_new(transport).unwrap(),
        })
        .unwrap(),
        classes: vec![Class::Oci],
        limits: Limits {
            connections: 1,
            message_bytes: wire::MAX_MESSAGE,
            in_flight: 4,
            programs: 4,
        },
        budgets: wire::Budgets {
            in_flight: 4,
            programs: 4,
        },
    };
    client.conn.shared.owed.lock().unwrap().bound = client.budgets.programs;
    client
}

struct NoDial {
    budget: Duration,
    dials: Arc<AtomicUsize>,
}
impl Link for NoDial {
    fn reply_budget(&self) -> Option<Duration> {
        Some(self.budget)
    }
    fn dial(&self, _: u32) -> Result<DuplexOwner, Refusal> {
        self.dials.fetch_add(1, Ordering::SeqCst);
        Err(Refusal::Broken("forbidden dial effect".into()))
    }
    fn dial_before(&self, port: u32, _: Option<Instant>) -> Result<DuplexOwner, Refusal> {
        self.dial(port)
    }
}

/// Do supply zero or an overflowing explicit budget; expect refusal before a request id,
/// transport write or Link dial is allocated/performed, rather than unbounded fallback.
#[test]
fn invalid_budgets_refuse_before_call_or_dial_effects() {
    for budget in [Duration::ZERO, Duration::MAX] {
        let (transport, request) = stalled(Some(budget));
        let client = configured(transport);
        assert!(matches!(
            client.list(),
            Err(Refusal::Invalid {
                what: "client reply budget",
                ..
            })
        ));
        let state = client.conn.shared.state().unwrap();
        assert_eq!(state.next, 0);
        assert!(state.calls.is_empty());
        assert!(state.programs.is_empty());
        drop(state);
        assert!(matches!(request.try_recv(), Err(mpsc::TryRecvError::Empty)));
        let dials = Arc::new(AtomicUsize::new(0));
        let link = guest::own_link(NoDial {
            budget,
            dials: Arc::clone(&dials),
        })
        .unwrap();
        assert!(matches!(
            guest::Client::connect(link.clone()),
            Err(Refusal::Invalid {
                what: "client reply budget",
                ..
            })
        ));
        assert_eq!(dials.load(Ordering::SeqCst), 0);
    }
}

/// Do queue an actual API call behind the existing whole-message sender; expect its own cap
/// to end acquisition with no write and no damage to the held sender or connection.
#[test]
fn queued_sender_consumes_its_own_budget_without_taking_authority() {
    /// The queued list's refusal at its own cap, the sender held.
    static CAPPED: Patience = Patience::measuring("test.hex_client.budget.queued_list_capped");
    let (transport, request) = stalled(Some(CAP));
    let client = Arc::new(configured(transport));
    let whole = client.conn.shared.sending.lock(None).unwrap().unwrap();
    let control = Arc::clone(&client);
    let (finished, answer) = mpsc::sync_channel(1);
    let worker = std::thread::spawn(move || finished.send(control.list()).unwrap());
    let result = recv_on(&CAPPED, &answer);
    // Release the exact owned guard even if the budget assertion fails, then join its waiter.
    drop(whole);
    if result.is_err() {
        client.conn.shared.end(Refusal::Broken(
            "owned queued-sender observation cleanup".into(),
        ));
    }
    worker.join().unwrap();
    let why = result.unwrap().unwrap_err();
    assert!(
        matches!(why, Refusal::Os { ref why, .. } if why.contains("caller") && !why.contains("twice"))
    );
    assert!(matches!(request.try_recv(), Err(mpsc::TryRecvError::Empty)));
    let state = client.conn.shared.state().unwrap();
    assert!(state.calls.is_empty());
    assert!(state.programs.is_empty());
    assert!(state.closed.is_none());
    assert!(!client.conn.shared.transport.closed.load(Ordering::Acquire));
}

/// Do omit the optional cap on an unarmed kind; expect the legacy wait to remain pending
/// until the exact connection is ended, rather than inheriting another client's cap.
#[test]
fn default_none_preserves_the_unarmed_wait() {
    /// The uncapped list's send.
    static SENT: Patience = Patience::measuring("test.hex_client.budget.uncapped_list_sent");
    let (transport, request) = stalled(None);
    let client = Arc::new(configured(transport));
    let control = Arc::clone(&client);
    let (finished, answer) = mpsc::sync_channel(1);
    let worker = std::thread::spawn(move || finished.send(control.list()).unwrap());
    let observed_send = recv_on(&SENT, &request);
    // Not a wait for a fact: CAP is another client's cap, which this call must not inherit, the claim under test.
    let before = answer.recv_timeout(CAP);
    client
        .conn
        .shared
        .end(Refusal::Broken("owned legacy-wait release".into()));
    worker.join().unwrap();
    observed_send.unwrap();
    assert!(matches!(before, Err(mpsc::RecvTimeoutError::Timeout)));
    assert!(matches!(
        answer.try_recv().unwrap(),
        Err(Refusal::Broken(_))
    ));
}

struct Calibrated<const LONG: bool>(Stalled);
impl<const LONG: bool> Transport for Calibrated<LONG> {
    type Reading = ();
    fn answers() -> &'static [Patience; KINDS] {
        static LONG_WAIT: [Patience; KINDS] =
            [const { Patience::seeded("test.client_budget.long", LONG_CAP) }; KINDS];
        static SHORT: [Patience; KINDS] =
            [const { Patience::seeded("test.client_budget.short", Duration::from_millis(1)) };
                KINDS];
        if LONG { &LONG_WAIT } else { &SHORT }
    }
    fn reply_budget(&self) -> Option<Duration> {
        self.0.reply_budget()
    }
    fn reading(&self) {}
    fn handle(&self) -> Handle<'_> {
        self.0.handle()
    }
    fn recv(&self, reading: &mut (), take: &mut Take<'_>) -> Result<bool, Refusal> {
        self.0.recv(reading, take)
    }
    fn try_send(&self, bytes: &[u8], at: &mut usize, lent: &[Lent<'_>]) -> Result<bool, Sent> {
        self.0.try_send(bytes, at, lent)
    }
    fn shutdown(&self) {
        self.0.shutdown();
    }
}

fn timed_call<T: Transport>(transport: T, request: mpsc::Receiver<()>) -> Refusal {
    /// The list's refusal at its caller's cap.
    static CAPPED: Patience = Patience::measuring("test.hex_client.budget.timed_list_capped");
    let client = Arc::new(configured(transport));
    let control = Arc::clone(&client);
    let (finished, answer) = mpsc::sync_channel(1);
    let worker = std::thread::spawn(move || finished.send(control.list()).unwrap());
    // Its send precedes its answer, so it is read once the answer has come.
    let result = recv_on(&CAPPED, &answer);
    let observed_send = request.try_recv();
    client
        .conn
        .shared
        .end(Refusal::Broken("owned calibrated control release".into()));
    worker.join().unwrap();
    observed_send.unwrap();
    result.unwrap().unwrap_err()
}

/// Do exhaust a caller's bound in actual Core calls, its learned wait ending once after it and
/// once before it; expect each refused as the caller's expiry, the learned wait before it a
/// run-out told to the census and never the call's end (AC-T7.4.1), after it nothing told. The
/// census runs in a fresh exact-test child so it never competes with the existing
/// refusal-census unit's OnceLock.
#[test]
fn timeout_cause_and_learning_keep_caller_authority_separate() {
    const CHILD: &str = "HEX_CLIENT_BUDGET_CENSUS_CHILD";
    if std::env::var_os(CHILD).is_none() {
        let mut child = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "calls::budget_tests::timeout_cause_and_learning_keep_caller_authority_separate",
                "--nocapture",
                "--test-threads=1",
            ])
            .env(CHILD, "1")
            .spawn()
            .unwrap();
        let pid = child.id();
        let status = child.wait().unwrap();
        eprintln!("client_budget_census child_pid={pid} reaped=true exit={status}");
        assert!(status.success());
        return;
    }
    static SAMPLES: AtomicUsize = AtomicUsize::new(0);
    static RUN_OUT: AtomicUsize = AtomicUsize::new(0);
    fn sink(name: &'static str, sample: hex_core::patience::Sample) {
        if name.starts_with("test.client_budget.") {
            SAMPLES.fetch_add(1, Ordering::SeqCst);
            if name == "test.client_budget.short"
                && matches!(sample, hex_core::patience::Sample::RanOut(_))
            {
                RUN_OUT.fetch_add(1, Ordering::SeqCst);
            }
        }
    }
    assert!(hex_core::patience::census(sink));
    let (transport, request) = stalled(Some(CAP));
    let caller = timed_call(Calibrated::<true>(transport), request);
    assert!(
        matches!(caller, Refusal::Os { why, .. } if why.contains("caller") && !why.contains("twice"))
    );
    assert_eq!(
        SAMPLES.load(Ordering::SeqCst),
        0,
        "caller expiry cannot calibrate a learned bound"
    );
    let (transport, request) = stalled(Some(CAP));
    let learned = timed_call(Calibrated::<false>(transport), request);
    assert!(
        matches!(learned, Refusal::Os { why, .. } if why.contains("caller") && !why.contains("twice"))
    );
    assert_eq!(SAMPLES.load(Ordering::SeqCst), 1);
    assert_eq!(RUN_OUT.load(Ordering::SeqCst), 1);
}

/// What [`late_census`] was told, by wait name: run-outs at their bounds, answers at their times.
static LATE_RAN_OUT: Mutex<Vec<(&'static str, Duration)>> = Mutex::new(Vec::new());
static LATE_ANSWERED: Mutex<Vec<(&'static str, Duration)>> = Mutex::new(Vec::new());
/// Whether a [`LateHello`] agent's answer is there to read: from its call's run-out on.
static LATE_DUE: AtomicBool = AtomicBool::new(false);
/// The agent's connection, written as its answer comes due, so a reader waiting on it reads it.
static LATE_WAKE: std::sync::OnceLock<UdpSocket> = std::sync::OnceLock::new();

fn late_census(name: &'static str, sample: hex_core::patience::Sample) {
    match sample {
        hex_core::patience::Sample::RanOut(after) => {
            LATE_RAN_OUT.lock().unwrap().push((name, after));
            LATE_DUE.store(true, Ordering::SeqCst);
            if let Some(wake) = LATE_WAKE.get() {
                wake.send(&[1]).unwrap();
            }
        }
        hex_core::patience::Sample::Answered(took) => {
            LATE_ANSWERED.lock().unwrap().push((name, took));
        }
        hex_core::patience::Sample::Refused(_) => {}
    }
}

/// Runs test `name` again alone in a child of this binary, [`late_census`] installed there: a census installs once a
/// process, and other tests' calls would reach it. True in the child, which runs the test; false in the parent once
/// the child has passed.
fn in_late_census_child(name: &str) -> bool {
    const CHILD: &str = "HEX_CLIENT_LATE_CENSUS_CHILD";
    if std::env::var_os(CHILD).is_some() {
        assert!(hex_core::patience::census(late_census));
        return true;
    }
    let mut child = std::process::Command::new(std::env::current_exe().unwrap())
        .args(["--exact", name, "--nocapture", "--test-threads=1"])
        .env(CHILD, "1")
        .spawn()
        .unwrap();
    let pid = child.id();
    let status = child.wait().unwrap();
    eprintln!("client_late_census child_pid={pid} reaped=true exit={status}");
    assert!(status.success(), "{name} failed in its census child");
    false
}

/// A stand-in agent busy past its record, as the guest's node was initializing its GPU: it answers a hello only once
/// the hello's learned wait, twice its record of 1 ms, has run out ([`LATE_DUE`], set by [`late_census`] where it is
/// installed). `CALLER` names its waits apart.
struct LateHello<const CALLER: bool> {
    io: UdpSocket,
    asked: Mutex<Option<u64>>,
}

fn late_hello<const CALLER: bool>() -> LateHello<CALLER> {
    let io = UdpSocket::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
    io.connect(io.local_addr().unwrap()).unwrap();
    io.set_nonblocking(true).unwrap();
    let _ = LATE_WAKE.set(io.try_clone().unwrap());
    LateHello {
        io,
        asked: Mutex::new(None),
    }
}

impl<const CALLER: bool> Transport for LateHello<CALLER> {
    type Reading = ();
    fn answers() -> &'static [Patience; KINDS] {
        static UNDER_CALLER: [Patience; KINDS] = [const {
            Patience::seeded("test.client_budget.late.caller", Duration::from_millis(1))
        }; KINDS];
        static ALONE: [Patience; KINDS] =
            [const { Patience::seeded("test.client_budget.late.alone", Duration::from_millis(1)) };
                KINDS];
        if CALLER { &UNDER_CALLER } else { &ALONE }
    }
    fn reading(&self) {}
    fn handle(&self) -> Handle<'_> {
        #[cfg(unix)]
        return std::os::fd::AsFd::as_fd(&self.io);
        #[cfg(windows)]
        std::os::windows::io::AsSocket::as_socket(&self.io)
    }
    fn recv(&self, _: &mut (), take: &mut Take<'_>) -> Result<bool, Refusal> {
        while self.io.recv(&mut [0]).is_ok() {}
        if !LATE_DUE.load(Ordering::SeqCst) {
            return Ok(false);
        }
        let Some(id) = self.asked.lock().unwrap().take() else {
            return Ok(false);
        };
        let hello = wire::Hello {
            build: wire::build(),
            classes: vec![Class::Oci],
            limits: Limits {
                connections: 1,
                message_bytes: wire::MAX_MESSAGE,
                in_flight: 1,
                programs: 1,
            },
            budgets: wire::Budgets {
                in_flight: 1,
                programs: 1,
            },
        };
        let bytes = wire::answer(id, &Answer::Hello(hello), wire::MAX_MESSAGE).unwrap();
        take(
            &Received {
                size: bytes.len(),
                bytes: &bytes,
                descriptors: 0,
                descriptors_cut: false,
            },
            Vec::new(),
        )?;
        Ok(true)
    }
    fn try_send(&self, bytes: &[u8], at: &mut usize, _: &[Lent<'_>]) -> Result<bool, Sent> {
        let (header, request) = wire::read_request_apart(bytes, wire::MAX_MESSAGE).unwrap();
        assert!(matches!(request, Request::Hello { .. }), "{request:?}");
        *self.asked.lock().unwrap() = Some(header.id);
        *at = bytes.len();
        Ok(true)
    }
    fn shutdown(&self) {
        let _ = self.io.send(&[1]);
    }
}

/// A caller's deadline no answer here comes near: the late answer is there to read as the learned wait runs out.
const LATE_CAP: Duration = Duration::from_secs(30);

/// AC-T7.4.1 (original): on 00aad a Firecracker guest with the AMD GPU passed through booted, and its relay's hello to
/// its own node, busy with the GPU's initialization, was refused, "none came in 2.354074ms, twice the slowest answer to
/// a call of its kind", while the boot had most of its 60 s left: the guest panicked. A learned bound is calibration,
/// not the caller's authority. Do: say hello under a caller's absolute deadline to an agent that answers only once the
/// hello's learned wait (twice its 1 ms record) has run out. Expect: the hello answered; its run-out told to the census
/// once, at the learned bound; its answer told too, later than that bound.
#[test]
fn original_a_late_answer_inside_its_callers_deadline_is_taken_its_run_out_recorded() {
    if !in_late_census_child(
        "calls::budget_tests::original_a_late_answer_inside_its_callers_deadline_is_taken_its_run_out_recorded",
    ) {
        return;
    }
    let shared = Shared::try_new(late_hello::<true>()).unwrap();
    let cap = ReplyDeadline::caller_before(Instant::now().checked_add(LATE_CAP).unwrap()).unwrap();
    let result = Core::hello_before(shared, Some(cap)).map(drop);
    let ran_out = LATE_RAN_OUT.lock().unwrap().clone();
    let answered = LATE_ANSWERED.lock().unwrap().clone();
    eprintln!(
        "client_late_answer under=caller result={result:?} ran_out={ran_out:?} answered={answered:?}"
    );
    assert!(
        result.is_ok(),
        "a slow answer inside its caller's deadline was refused: {result:?}"
    );
    let bound = Duration::from_millis(2);
    assert_eq!(ran_out, [("test.client_budget.late.caller", bound)]);
    assert!(
        matches!(answered.as_slice(), [("test.client_budget.late.caller", took)] if *took > bound),
        "{answered:?}"
    );
}

/// AC-T7.4.1: with no caller's deadline a call's learned wait is still its bound, its hang detection. Do: say hello with
/// no caller deadline to the late agent, which no census tells its wait ran out here, so it never answers. Expect:
/// refused as twice the slowest answer, at its 2 ms bound.
#[test]
fn with_no_callers_deadline_the_learned_wait_still_refuses() {
    let shared = Shared::try_new(late_hello::<false>()).unwrap();
    let result = Core::hello_before(shared, None).map(drop);
    eprintln!("client_late_answer under=none result={result:?}");
    assert!(
        matches!(&result, Err(Refusal::Os { why, .. }) if why.contains("none came in 2ms, twice the slowest")),
        "{result:?}"
    );
}

fn until(at: Instant) -> Result<(), Refusal> {
    let timer = Wake::new().map_err(|e| Refusal::os("making the fixture timer wake", e))?;
    loop {
        let left = at.saturating_duration_since(Instant::now());
        if left.is_zero() {
            return Ok(());
        }
        // The Condvar fixture wait overshot the injection event in retained08a diagnostics.
        // Use the same readiness clock as the protocol, without changing its cap or guards.
        ready(timer.handle(), false, None, Some(left))?;
    }
}

#[derive(Clone, Copy, Debug)]
struct TokenRead {
    at: Instant,
    result: Result<usize, io::ErrorKind>,
}

struct ReadNotice {
    socket: TcpStream,
    read: mpsc::SyncSender<TokenRead>,
}
impl Read for ReadNotice {
    fn read(&mut self, bytes: &mut [u8]) -> io::Result<usize> {
        let want = bytes.len().min(1);
        let result = self.socket.read(&mut bytes[..want]);
        self.read
            .try_send(TokenRead {
                at: Instant::now(),
                result: result.as_ref().copied().map_err(io::Error::kind),
            })
            .map_err(|error| io::Error::other(format!("token observation queue: {error}")))?;
        result
    }
}
impl Write for ReadNotice {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        self.socket.write(bytes)
    }
    fn flush(&mut self) -> io::Result<()> {
        self.socket.flush()
    }
}
impl Duplex for ReadNotice {
    fn try_clone(&self) -> io::Result<DuplexOwner> {
        own_duplex(Self {
            socket: self.socket.try_clone()?,
            read: self.read.clone(),
        })
        .map_err(|_| io::Error::from(io::ErrorKind::OutOfMemory))
    }
    fn shutdown(&self, how: Shutdown) -> io::Result<()> {
        self.socket.shutdown(how)
    }
    fn set_write_timeout(&self, timeout: Option<Duration>) -> io::Result<()> {
        self.socket.set_write_timeout(timeout)
    }
    fn set_read_timeout(&self, timeout: Option<Duration>) -> io::Result<()> {
        self.socket.set_read_timeout(timeout)
    }
    fn handle(&self) -> Handle<'_> {
        #[cfg(unix)]
        return std::os::fd::AsFd::as_fd(&self.socket);
        #[cfg(windows)]
        std::os::windows::io::AsSocket::as_socket(&self.socket)
    }
    fn set_nonblocking(&self, on: bool) -> io::Result<()> {
        self.socket.set_nonblocking(on)
    }
}

struct RecordedLink {
    control: SocketAddr,
    stdio: SocketAddr,
    budget: Duration,
    deadlines: Arc<Mutex<Vec<(u32, Instant)>>>,
    read: Option<mpsc::SyncSender<TokenRead>>,
}
impl Link for RecordedLink {
    fn reply_budget(&self) -> Option<Duration> {
        Some(self.budget)
    }
    fn dial(&self, port: u32) -> Result<DuplexOwner, Refusal> {
        self.dial_before(port, Some(Instant::now().checked_add(self.budget).unwrap()))
    }
    fn dial_before(&self, port: u32, deadline: Option<Instant>) -> Result<DuplexOwner, Refusal> {
        let deadline = deadline.unwrap();
        self.deadlines.lock().unwrap().push((port, deadline));
        let address = if port == hex_core::stream::CONTROL_PORT {
            self.control
        } else {
            self.stdio
        };
        let socket = TcpStream::connect_timeout(
            &address,
            deadline.saturating_duration_since(Instant::now()),
        )
        .map_err(|error| Refusal::os("recorded loopback dial", error))?;
        // These controls deliberately send individual bytes/small frames; TCP coalescing is
        // unrelated to whether protocol progress consumes one deadline.
        socket.set_nodelay(true).unwrap();
        if port == hex_core::stream::CONTROL_PORT
            && let Some(read) = &self.read
        {
            own_duplex(ReadNotice {
                socket,
                read: read.clone(),
            })
        } else {
            own_duplex(socket)
        }
    }
}

/// Accepts a dial on `listener` by `at`, or with no deadline whenever it comes, unless `stop` is set (and `wake`
/// written) first.
fn accept_before(
    listener: &TcpListener,
    stop: &AtomicBool,
    wake: &Wake,
    at: Option<Instant>,
) -> io::Result<TcpStream> {
    loop {
        if stop.load(Ordering::Acquire) {
            return Err(io::ErrorKind::Interrupted.into());
        }
        match listener.accept() {
            Ok((peer, _)) => {
                peer.set_nonblocking(false)?;
                return Ok(peer);
            }
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => {}
            Err(error) => return Err(error),
        }
        let left = timeout_by(at)?;
        #[cfg(unix)]
        let handle = std::os::fd::AsFd::as_fd(listener);
        #[cfg(windows)]
        let handle = std::os::windows::io::AsSocket::as_socket(listener);
        ready(handle, false, Some(wake), left)
            .map_err(|error| io::Error::other(error.to_string()))?;
    }
}

fn actual_token_read(
    reads: &mpsc::Receiver<TokenRead>,
    events: &mut Vec<TokenRead>,
    at: Option<Instant>,
) -> Result<Instant, Refusal> {
    loop {
        let event = recv_by(reads, at)
            .map_err(|error| Refusal::os("observing an actual token read", error))?;
        events.push(event);
        match event.result {
            Ok(n) if n > 0 => return Ok(event.at),
            Err(io::ErrorKind::WouldBlock | io::ErrorKind::Interrupted) => {}
            other => return Err(Refusal::Broken(format!("the token probe read {other:?}"))),
        }
    }
}

/// Do make real partial token progress just before the original deadline; expect expiry at
/// that deadline, rather than another whole cap after the most recent byte.
#[test]
fn partial_token_progress_does_not_reset_the_absolute_deadline() {
    /// The connect's dial, accepted.
    static DIALED: Patience = Patience::measuring("test.hex_client.budget.partial_token_dialed");
    /// The client's read of the token's first byte.
    static FIRST_READ: Patience =
        Patience::measuring("test.hex_client.budget.partial_token_first_read");
    /// The connect's refusal at its original deadline, the token cut short.
    static EXPIRED: Patience = Patience::measuring("test.hex_client.budget.partial_token_expired");
    let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
    listener.set_nonblocking(true).unwrap();
    let address = listener.local_addr().unwrap();
    let stop = AtomicBool::new(false);
    let wake = Wake::new().unwrap();
    // The fixed token has eight bytes; this two-byte case records positive and WouldBlock reads.
    let (read, reads) = mpsc::sync_channel(hex_core::stream::TOKEN_BYTES * 2);
    let recorded_deadlines = Arc::new(Mutex::new(Vec::new()));
    let link = guest::own_link(RecordedLink {
        control: address,
        stdio: address,
        budget: LONG_CAP,
        deadlines: Arc::clone(&recorded_deadlines),
        read: Some(read),
    })
    .unwrap();
    let (finished, answer) = mpsc::sync_channel(1);
    // The worker holds the link alone, so a client that gives up takes every read notice's sender with it: a read
    // wait then finds its sender gone rather than waiting for a read that won't come.
    let worker = std::thread::spawn(move || {
        let result = guest::Client::connect(link).map(drop);
        finished.send((Instant::now(), result)).unwrap()
    });
    let mut peer = None;
    let mut events = Vec::new();
    let observation = (|| -> Result<_, Refusal> {
        peer = Some(
            within(&DIALED, |by| accept_before(&listener, &stop, &wake, by))
                .map_err(|error| Refusal::os("accepting the owned token peer", error))?,
        );
        let peer = peer.as_mut().unwrap();
        peer.set_nodelay(true)
            .map_err(|error| Refusal::os("configuring token probe TCP", error))?;
        peer.write_all(&[1])
            .map_err(|error| Refusal::os("writing the first token byte", error))?;
        let first = within(&FIRST_READ, |by| actual_token_read(&reads, &mut events, by))?;
        let deadline = recorded_deadlines
            .lock()
            .unwrap()
            .first()
            .copied()
            .ok_or_else(|| Refusal::Broken("the token peer had no recorded dial".into()))?
            .1;
        until(deadline.checked_sub(CAP).unwrap())?;
        if Instant::now() >= deadline {
            return Err(Refusal::Broken(
                "harness miss: the original token injection window elapsed".into(),
            ));
        }
        peer.write_all(&[2])
            .map_err(|error| Refusal::os("writing the second token byte", error))?;
        // Not a wait for a fact: the claim's window. This read must come within CAP of its injection, by the original
        // deadline, and past that deadline the client reads no more.
        let second = actual_token_read(
            &reads,
            &mut events,
            Some(Instant::now().checked_add(CAP).unwrap()),
        )?;
        Ok((first, second, deadline))
    })();
    let completed = recv_on(&EXPIRED, &answer);
    // Every observation error, including the first event/dial failure, reaches this cleanup.
    let cleanup = peer.as_ref().map(|peer| peer.shutdown(Shutdown::Both));
    drop(peer);
    stop.store(true, Ordering::Release);
    wake.wake();
    worker.join().unwrap();
    let after_cleanup = completed.is_err().then(|| answer.try_recv().ok()).flatten();
    events.extend(reads.try_iter());
    let witnessed = observation
        .as_ref()
        .is_ok_and(|(first, second, deadline)| first < second && second < deadline);
    eprintln!(
        "client_budget_token harness_miss={} observation={observation:?} actual_reads={events:?} client_before_cleanup={completed:?} client_after_cleanup={after_cleanup:?}",
        !witnessed
    );
    assert!(cleanup.is_none_or(|result| result.is_ok()
        || matches!(result, Err(error) if error.kind() == io::ErrorKind::NotConnected)));
    let (first, second, deadline) = observation.unwrap();
    assert!(
        first < second && second < deadline,
        "both actual partial reads must precede the original deadline"
    );
    assert!(
        events
            .iter()
            .any(|event| matches!(event.result, Err(io::ErrorKind::WouldBlock)))
    );
    let (returned, result) = completed.unwrap();
    assert!(matches!(result, Err(Refusal::Os { why, .. }) if why.contains("caller")));
    // A deadline that progress reset would end no sooner than a whole budget after the most recent byte's read.
    assert!(
        returned < second.checked_add(LONG_CAP).unwrap(),
        "the connect ended {:?} after its most recent byte: progress reset its original deadline",
        returned.saturating_duration_since(second)
    );
    assert_eq!(recorded_deadlines.lock().unwrap().len(), 1);
}

fn next_request(peer: &mut TcpStream) -> io::Result<(Header, Request)> {
    let mut prefix = [0; size_of::<u32>()];
    peer.read_exact(&mut prefix)?;
    let length = usize::try_from(u32::from_be_bytes(prefix)).unwrap();
    assert!(length <= wire::MAX_MESSAGE);
    let mut bytes = vec![0; length];
    peer.read_exact(&mut bytes)?;
    Ok(wire::read_request_apart(&bytes, wire::MAX_MESSAGE).unwrap())
}
fn answer_to(peer: &mut TcpStream, id: u64, answer: Answer) -> io::Result<()> {
    peer.write_all(
        &hex_core::stream::frame(
            &wire::answer(id, &answer, wire::MAX_MESSAGE).unwrap(),
            wire::MAX_MESSAGE,
        )
        .unwrap(),
    )
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ControlPhase {
    Start,
    Delete,
    Disconnected,
    Stop,
}

/// Waits for the owner to say `expected` is pending, then for what the client sends for it, read by `read` on that
/// fact's own patience. Workload idle is not a protocol wait: the owner always sends Stop or closes peers on error, and
/// the read's wait starts only when a control operation is actually pending.
fn pending_control<R>(
    peer: &mut TcpStream,
    phases: &mpsc::Receiver<ControlPhase>,
    expected: ControlPhase,
    read: impl FnOnce(&mut TcpStream) -> io::Result<R>,
) -> io::Result<R> {
    /// What the client sends for an announced control operation: its request, or for a disconnect its end.
    static ASKED: Patience = Patience::measuring("test.hex_client.budget.named_pending_asked");
    match phases.recv() {
        Ok(phase) if phase == expected => within(&ASKED, |by| {
            peer.set_read_timeout(timeout_by(by)?)?;
            read(peer)
        }),
        Ok(ControlPhase::Stop) | Err(_) => Err(io::ErrorKind::Interrupted.into()),
        Ok(other) => Err(io::Error::other(format!(
            "expected {expected:?}, got {other:?}"
        ))),
    }
}

enum StreamPhase {
    WriteLate,
    Stop,
}

/// Do ask for all three streams in one actual guest exec; expect one common setup deadline,
/// later API use after it expired, and successful stdio/Proc completion beyond the cap.
#[test]
fn named_streams_share_one_operation_cap_without_a_workload_or_connection_timer() {
    use hex_core::stream::{self, Named};
    /// The client's control dial, accepted by the agent.
    static CONTROL_DIALED: Patience =
        Patience::measuring("test.hex_client.budget.named_control_dialed");
    /// The exec's request, come to the agent.
    static EXEC_ASKED: Patience = Patience::measuring("test.hex_client.budget.named_exec_asked");
    /// The exec's three named dials, accepted and each named.
    static STREAMS_DIALED: Patience =
        Patience::measuring("test.hex_client.budget.named_streams_dialed");
    /// The late stdout bytes, read through the exec's own stream.
    static LATE_READ: Patience = Patience::measuring("test.hex_client.budget.named_late_read");
    /// The agent's own end, once the last client has gone.
    static AGENT_ENDED: Patience = Patience::measuring("test.hex_client.budget.named_agent_ended");
    let control = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
    let stdio = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
    control.set_nonblocking(true).unwrap();
    stdio.set_nonblocking(true).unwrap();
    let stop = Arc::new(AtomicBool::new(false));
    let wake = Arc::new(Wake::new().unwrap());
    let owned = Arc::new(Mutex::new(Vec::<TcpStream>::new()));
    let recorded_deadlines = Arc::new(Mutex::new(Vec::new()));
    let link = guest::own_link(RecordedLink {
        control: control.local_addr().unwrap(),
        stdio: stdio.local_addr().unwrap(),
        budget: CAP,
        deadlines: Arc::clone(&recorded_deadlines),
        read: None,
    })
    .unwrap();
    let (agent_stop, agent_wake, agent_owned) =
        (Arc::clone(&stop), Arc::clone(&wake), Arc::clone(&owned));
    let (agent_finished, agent_done) = mpsc::sync_channel(1);
    // At most one pending operation followed by Stop can await this actor.
    let (phase, pending) = mpsc::sync_channel(2);
    let agent = std::thread::spawn(move || {
        let result = (|| -> io::Result<()> {
            let mut peer = within(&CONTROL_DIALED, |by| {
                accept_before(&control, &agent_stop, &agent_wake, by)
            })?;
            agent_owned.lock().unwrap().push(peer.try_clone()?);
            peer.set_nodelay(true)?;
            peer.write_all(&11u64.to_le_bytes())?;
            let (exec, request) = within(&EXEC_ASKED, |by| {
                peer.set_read_timeout(timeout_by(by)?)?;
                next_request(&mut peer)
            })?;
            eprintln!("client_budget_named phase=agent_exec id={}", exec.id);
            assert!(matches!(
                request,
                Request::Call(Call::Exec {
                    streams: Streams {
                        stdin: true,
                        stdout: true,
                        stderr: true
                    },
                    ..
                })
            ));
            answer_to(&mut peer, exec.id, Answer::Started { pid: 7 })?;
            let (header, request) =
                pending_control(&mut peer, &pending, ControlPhase::Start, next_request)?;
            eprintln!("client_budget_named phase=agent_start id={}", header.id);
            assert!(matches!(request, Request::Call(Call::Start(_))));
            answer_to(&mut peer, header.id, Answer::Done)?;
            let (header, request) =
                pending_control(&mut peer, &pending, ControlPhase::Delete, next_request)?;
            assert!(matches!(request, Request::Call(Call::Delete(_))));
            answer_to(&mut peer, header.id, Answer::Done)?;
            answer_to(&mut peer, exec.id, Answer::Exited(Exit::Code(0)))?;
            eprintln!(
                "client_budget_named phase=agent_exited_written id={}",
                exec.id
            );
            // A completed Proc sends no Release; dropping the last client closes this control.
            let end = pending_control(&mut peer, &pending, ControlPhase::Disconnected, |peer| {
                peer.read(&mut [0])
            })?;
            assert_eq!(end, 0);
            Ok(())
        })();
        let _ = agent_finished.send(());
        result
    });
    // One WriteLate and then Stop; neither idle phase is a workload lifetime timer.
    let (late, stream_phase) = mpsc::sync_channel(2);
    let (streams_stop, streams_wake, streams_owned) =
        (Arc::clone(&stop), Arc::clone(&wake), Arc::clone(&owned));
    let streams = std::thread::spawn(move || -> io::Result<Vec<Named>> {
        // One setup's three dials, one fact: one wait for all three accepted and named.
        let mut held = within(&STREAMS_DIALED, |by| -> io::Result<Vec<_>> {
            let mut held = Vec::new();
            for _ in 0..3 {
                let mut peer = accept_before(&stdio, &streams_stop, &streams_wake, by)?;
                streams_owned.lock().unwrap().push(peer.try_clone()?);
                peer.set_nodelay(true)?;
                peer.set_read_timeout(timeout_by(by)?)?;
                let mut name = [0; stream::NAME_BYTES];
                peer.read_exact(&mut name)?;
                held.push((stream::read_name(&name).unwrap(), peer));
            }
            Ok(held)
        })?;
        if matches!(stream_phase.recv(), Ok(StreamPhase::WriteLate)) {
            for (name, peer) in &mut held {
                if name.stream == stream::Stdio::Out {
                    peer.write_all(b"late")?;
                }
            }
            let _ = stream_phase.recv();
        }
        Ok(held.into_iter().map(|(name, _)| name).collect())
    });
    let mut reading = None;
    let result = (|| -> Result<_, Refusal> {
        let transport = guest::Stream::dial(link.clone())?;
        let client = configured(transport);
        let id = BoxId::new("budget-unit")?;
        let mut proc = client.exec(
            &id,
            Program::of(&["cat"]),
            Stdio {
                ask: hex_api::Ask {
                    stdin: true,
                    stdout: true,
                    stderr: true,
                },
                ..Stdio::null()
            },
        )?;
        eprintln!("client_budget_named phase=client_started");
        let deadlines = recorded_deadlines.lock().unwrap().clone();
        let deadline = deadlines.last().unwrap().1;
        until(deadline)?;
        let pending = proc.try_wait()?;
        eprintln!("client_budget_named phase=client_proc_pending value={pending:?}");
        phase
            .send(ControlPhase::Start)
            .map_err(|error| Refusal::os("announcing pending fixture start", error))?;
        client.start(&id)?;
        late.send(StreamPhase::WriteLate)
            .map_err(|error| Refusal::os("requesting late fixture stdio", error))?;
        let given = proc.streams()?;
        let mut stdout = hex_api::OwnedStream::from(
            given
                .stdout
                .ok_or_else(|| Refusal::Broken("fixture stdout was absent".into()))?,
        );
        let (read, bytes) = mpsc::sync_channel(1);
        reading = Some(std::thread::spawn(move || {
            let mut bytes = [0; 4];
            let result = stdout.read_exact(&mut bytes).map(|()| bytes);
            let _ = read.send(result);
        }));
        let bytes = recv_on(&LATE_READ, &bytes)
            .map_err(|error| Refusal::os("observing requested late fixture bytes", error))?
            .map_err(|error| Refusal::os("reading late fixture stdio", error))?;
        eprintln!("client_budget_named phase=client_late_stdio_read");
        phase
            .send(ControlPhase::Delete)
            .map_err(|error| Refusal::os("announcing pending fixture delete", error))?;
        client.delete(&id)?;
        // A program's end through a Proc takes the suites' program-end record (hex_testkit::waits).
        let ended = proc.wait_for(hex_api::conformance::PATIENCE)?;
        drop(proc);
        let clean = {
            let state = client.conn.shared.state()?;
            state.calls.is_empty() && state.programs.is_empty() && state.given.is_empty()
        };
        phase
            .send(ControlPhase::Disconnected)
            .map_err(|error| Refusal::os("announcing fixture disconnect", error))?;
        drop(client);
        Ok((deadlines, pending, bytes, ended, clean))
    })();
    let _ = phase.send(ControlPhase::Stop);
    let _ = late.send(StreamPhase::Stop);
    // A failed dial or setup can leave an accept waiting on a dial that won't come: wake both accept loops before
    // waiting on the agent's end.
    stop.store(true, Ordering::Release);
    wake.wake();
    let natural_agent_end = recv_on(&AGENT_ENDED, &agent_done);
    if result.is_err() || natural_agent_end.is_err() {
        for peer in owned.lock().unwrap().iter() {
            let _ = peer.shutdown(Shutdown::Both);
        }
    }
    let names = streams.join().unwrap();
    let agent_result = agent.join().unwrap();
    if let Some(reading) = reading {
        reading.join().unwrap();
    }
    eprintln!(
        "client_budget_named result={result:?} agent={agent_result:?} named_count={:?}",
        names.as_ref().map(Vec::len)
    );
    // No assertion can strand either owned accept/read actor.
    let (deadlines, pending, bytes, ended, clean) = result.unwrap();
    natural_agent_end.unwrap();
    let names = names.unwrap();
    agent_result.unwrap();
    assert_eq!(
        deadlines.len(),
        4,
        "one control dial, then three named dials"
    );
    assert!(
        deadlines[1..]
            .iter()
            .all(|(port, at)| *port == stream::STDIO_PORT && *at == deadlines[1].1)
    );
    assert_eq!(pending, None);
    assert_eq!(&bytes, b"late");
    assert_eq!(ended, Some(Exit::Code(0)));
    assert!(clean);
    assert_eq!(names.len(), 3);
    assert!(
        names
            .iter()
            .all(|name| name.exec.conn == 11 && name.exec.id == names[0].exec.id)
    );
}

/// What the offered-expiry control sees happen: its open begin, and its exec end.
enum Offered {
    Opening,
    Ended(Result<(), Refusal>),
}

struct Offers {
    io: Stalled,
    inbound: Mutex<VecDeque<Vec<u8>>>,
    outbound: Mutex<Vec<(u64, Request)>>,
    seen: mpsc::Sender<Offered>,
    /// The open holds until the sibling call has completed, a fact the test sends.
    sibling: Mutex<mpsc::Receiver<()>>,
    program: Mutex<Option<Stdio>>,
}
impl Offers {
    /// One whose open no test watches, for the fixtures that wrap it and never open an offer.
    fn unobserved(io: Stalled) -> Self {
        let (seen, _) = mpsc::channel();
        let (_, sibling) = mpsc::channel();
        Self {
            io,
            inbound: Mutex::new(VecDeque::new()),
            outbound: Mutex::new(Vec::new()),
            seen,
            sibling: Mutex::new(sibling),
            program: Mutex::new(None),
        }
    }

    fn queue(&self, id: u64, answer: Answer) {
        self.inbound
            .lock()
            .unwrap()
            .push_back(wire::answer(id, &answer, wire::MAX_MESSAGE).unwrap());
        self.io.socket.send(&[1]).unwrap();
    }
}
impl Transport for Offers {
    type Reading = ();
    fn answers() -> &'static [Patience; KINDS] {
        Stalled::answers()
    }
    fn reply_budget(&self) -> Option<Duration> {
        Some(LONG_CAP)
    }
    fn reading(&self) {}
    fn handle(&self) -> Handle<'_> {
        self.io.handle()
    }
    fn recv(&self, _: &mut (), take: &mut Take<'_>) -> Result<bool, Refusal> {
        while self.io.socket.recv(&mut [0]).is_ok() {}
        let mut came = false;
        loop {
            let Some(bytes) = self.inbound.lock().unwrap().pop_front() else {
                return Ok(came);
            };
            take(
                &Received {
                    size: bytes.len(),
                    bytes: &bytes,
                    descriptors: 0,
                    descriptors_cut: false,
                },
                Vec::new(),
            )?;
            came = true;
        }
    }
    fn try_send(&self, bytes: &[u8], at: &mut usize, _: &[Lent<'_>]) -> Result<bool, Sent> {
        let (header, request) = wire::read_request_apart(bytes, wire::MAX_MESSAGE).unwrap();
        match &request {
            Request::Call(Call::ExecStreams { .. }) => self.queue(
                header.id,
                Answer::Offered(wire::StreamOffer {
                    pid: 1,
                    started: 1,
                    vsock: "owned-budget-offer".into(),
                    token: 1,
                    exec: header.id,
                    streams: Streams {
                        stdin: true,
                        ..Streams::default()
                    },
                }),
            ),
            Request::Call(Call::Start(_)) => self.queue(header.id, Answer::Done),
            Request::Release => {}
            other => panic!("unexpected budget offer request: {other:?}"),
        }
        self.outbound.lock().unwrap().push((header.id, request));
        *at = bytes.len();
        Ok(true)
    }
    fn open_offer_before(
        &self,
        _: &wire::StreamOffer,
        deadline: ReplyDeadline,
    ) -> Result<hex_api::Given, Refusal> {
        let mut program = Stdio {
            ask: hex_api::Ask {
                stdin: true,
                stdout: false,
                stderr: false,
            },
            ..Stdio::null()
        };
        let given = program.make_asked()?;
        *self.program.lock().unwrap() = Some(program);
        self.seen.send(Offered::Opening).unwrap();
        // Held without the reader's turn until the sibling has completed (or its sender is gone),
        // then until the open's own deadline has passed, so its refusal is the caller's expiry.
        let _ = self.sibling.lock().unwrap().recv();
        until(deadline.at())?;
        drop(given);
        Err(deadline
            .remaining("opening budgeted offered streams")
            .unwrap_err())
    }
    fn shutdown(&self) {
        self.io.shutdown();
    }
}

/// Do stall one bounded offered-stream open until a sibling call has completed, then past its
/// deadline; expect the sibling answered, the open's caller expiry, a same-id Release, its made
/// ends closed and no call/program/given reservation left. Each step waits on the fact before it,
/// never on an observation window (AC-T7.4.1).
#[test]
fn offered_expiry_releases_only_its_ownership_while_a_sibling_progresses() {
    /// The worker's first event: its open begun, or its exec ended without one.
    static FIRST: Patience = Patience::measuring("test.hex_client.budget.offered_first");
    /// The exec's end, its open refused at the caller's expiry.
    static ENDED: Patience = Patience::measuring("test.hex_client.budget.offered_ended");
    let (io, _) = stalled(Some(LONG_CAP));
    let (seen, events) = mpsc::channel();
    let (sibling_done, sibling_wait) = mpsc::channel();
    let client = Arc::new(configured(Offers {
        io,
        inbound: Mutex::new(VecDeque::new()),
        outbound: Mutex::new(Vec::new()),
        seen: seen.clone(),
        sibling: Mutex::new(sibling_wait),
        program: Mutex::new(None),
    }));
    let control = Arc::clone(&client);
    let id = BoxId::new("offered-budget-unit").unwrap();
    let target = id.clone();
    let worker = std::thread::spawn(move || {
        // A fixture panic is this worker's end too, so the test's wait for it always ends.
        let ended = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            control
                .exec(
                    &target,
                    Program::of(&["cat"]),
                    Stdio {
                        ask: hex_api::Ask {
                            stdin: true,
                            stdout: false,
                            stderr: false,
                        },
                        ..Stdio::null()
                    },
                )
                .map(drop)
        }))
        .unwrap_or_else(|_| Err(Refusal::Broken("the exec's fixture panicked".into())));
        seen.send(Offered::Ended(ended)).unwrap();
    });
    // The open began, or the exec ended without one: both are facts the worker sends.
    let first = recv_on(&FIRST, &events).unwrap();
    let opened = matches!(first, Offered::Opening);
    let reading = client.conn.shared.state().unwrap().reading;
    let sibling = client.start(&id);
    sibling_done.send(()).unwrap();
    let ended = match first {
        Offered::Ended(ended) => ended,
        Offered::Opening => match recv_on(&ENDED, &events).unwrap() {
            Offered::Ended(ended) => ended,
            Offered::Opening => panic!("the exec opened its offer twice"),
        },
    };
    worker.join().unwrap();
    let shared = &client.conn.shared;
    let outbound = shared.transport.outbound.lock().unwrap();
    let state = shared.state().unwrap();
    let clean = state.calls.is_empty()
        && state.programs.is_empty()
        && state.given.is_empty()
        && state.closed.is_none();
    drop(state);
    // A regression that kept ends in the owned ledger would hold the program's stdin open: clear
    // only that ledger first, so the read below can't wait on it; `clean` still fails the test.
    shared.state().unwrap().given.clear();
    let program = shared.transport.program.lock().unwrap().take();
    let closed = program
        .and_then(|program| program.stdin)
        .map(|mut stdin| stdin.read(&mut [0]).unwrap());
    assert!(opened, "the exec ended before its offered open began");
    assert!(!reading, "an offered open must relinquish the reader turn");
    assert_eq!(sibling, Ok(()));
    assert!(matches!(ended, Err(Refusal::Os { why, .. }) if why.contains("caller")));
    assert!(clean);
    assert_eq!(closed, Some(0));
    assert_eq!(outbound.len(), 3);
    assert_eq!(outbound[0].0, outbound[2].0);
    assert!(matches!(outbound[1].1, Request::Call(Call::Start(_))));
    assert_eq!(outbound[2].1, Request::Release);
}

fn absolute_spec() -> BoxSpec {
    BoxSpec {
        id: BoxId::new("absolute-operation-control").unwrap(),
        bounds: hex_api::Bounds::default(),
        class: hex_api::ClassSpec::Process,
        owner: None,
        devices: Vec::new(),
        allocation: None,
        route: None,
        band: 0,
    }
}

/// A private, initially Measuring table so these controls never depend on other tests'
/// process-wide census samples. It retains the existing Stalled I/O/cleanup fixture.
struct AbsoluteStalled(Stalled);
impl Transport for AbsoluteStalled {
    type Reading = ();
    fn answers() -> &'static [Patience; KINDS] {
        static WAITS: [Patience; KINDS] =
            [const { Patience::measuring("test.client_absolute.unarmed") }; KINDS];
        &WAITS
    }
    fn reply_budget(&self) -> Option<Duration> {
        self.0.reply_budget()
    }
    fn reading(&self) {}
    fn handle(&self) -> Handle<'_> {
        self.0.handle()
    }
    fn recv(&self, reading: &mut (), take: &mut Take<'_>) -> Result<bool, Refusal> {
        self.0.recv(reading, take)
    }
    fn try_send(&self, bytes: &[u8], at: &mut usize, lent: &[Lent<'_>]) -> Result<bool, Sent> {
        self.0.try_send(bytes, at, lent)
    }
    fn shutdown(&self) {
        self.0.shutdown();
    }
}

/// AC-T7.4.1: an expired absolute operation refuses before call/program IDs,
/// writes, or stream setup; the same client remains usable by its owner.
#[test]
fn absolute_operations_reject_expired_before_effects() {
    let (transport, request) = stalled(None);
    let client = configured(AbsoluteStalled(transport));
    let expired = Instant::now();
    let run = client.run_before(
        absolute_spec(),
        Program::of(&["unused"]),
        Stdio::null(),
        expired,
    );
    let delete = client.delete_before(&BoxId::new("absolute-operation-control").unwrap(), expired);
    assert!(matches!(run, Err(Refusal::Os { why, .. }) if why.contains("absolute")));
    assert!(matches!(delete, Err(Refusal::Os { why, .. }) if why.contains("absolute")));
    assert!(matches!(request.try_recv(), Err(mpsc::TryRecvError::Empty)));
    let state = client.conn.shared.state().unwrap();
    assert_eq!(state.next, 0);
    assert!(state.calls.is_empty() && state.programs.is_empty() && state.given.is_empty());
    assert!(state.closed.is_none());
}

/// AC-T7.4.1: run/delete use the actual Core answer path with no configured Link
/// cap and an unarmed Measuring wait. Caller expiry is not a learned sample.
#[test]
fn absolute_operations_cap_unarmed_answers_without_link_defaults() {
    /// The run's or the delete's refusal at its absolute cap.
    static CAPPED: Patience = Patience::measuring("test.hex_client.budget.absolute_op_capped");
    for run in [true, false] {
        let (transport, request) = stalled(None);
        let client = Arc::new(configured(AbsoluteStalled(transport)));
        let control = Arc::clone(&client);
        let (finished, answer) = mpsc::sync_channel(1);
        let worker = std::thread::spawn(move || {
            let deadline = Instant::now().checked_add(CAP).unwrap();
            let result = if run {
                control
                    .run_before(
                        absolute_spec(),
                        Program::of(&["unused"]),
                        Stdio::null(),
                        deadline,
                    )
                    .map(drop)
            } else {
                control.delete_before(&BoxId::new("absolute-operation-control").unwrap(), deadline)
            };
            let _ = finished.send(result);
        });
        // Its send precedes its answer, so it is read once the answer has come.
        let result = recv_on(&CAPPED, &answer);
        let observed_send = request.try_recv();
        // Every observation failure still closes this exact connection and joins its actor.
        client.conn.shared.end(Refusal::Broken(
            "absolute operation observation cleanup".into(),
        ));
        let joined = worker.join();
        eprintln!(
            "client_absolute run={run} observed_send={observed_send:?} result={result:?} joined={}",
            joined.is_ok()
        );
        joined.unwrap();
        observed_send.unwrap();
        assert!(
            matches!(result.unwrap(), Err(Refusal::Os { why, .. }) if why.contains("absolute") && !why.contains("twice"))
        );
        let state = client.conn.shared.state().unwrap();
        assert!(state.calls.is_empty() && state.programs.is_empty() && state.given.is_empty());
    }
    assert!(
        AbsoluteStalled::answers()
            .iter()
            .all(|wait| wait.deadline().is_none())
    );
}

/// AC-T7.4.1: the same caller deadline bounds sender ownership without taking
/// another sender's guard or positively reporting remote cleanup.
#[test]
fn absolute_delete_cap_covers_queued_sender() {
    /// The queued delete's refusal at its absolute cap, the sender held.
    static CAPPED: Patience = Patience::measuring("test.hex_client.budget.queued_delete_capped");
    let (transport, request) = stalled(None);
    let client = Arc::new(configured(AbsoluteStalled(transport)));
    let whole = client.conn.shared.sending.lock(None).unwrap().unwrap();
    let control = Arc::clone(&client);
    let (finished, answer) = mpsc::sync_channel(1);
    let worker = std::thread::spawn(move || {
        let deadline = Instant::now().checked_add(CAP).unwrap();
        let _ = finished.send(
            control.delete_before(&BoxId::new("absolute-operation-control").unwrap(), deadline),
        );
    });
    let result = recv_on(&CAPPED, &answer);
    drop(whole);
    if result.is_err() {
        client
            .conn
            .shared
            .end(Refusal::Broken("absolute queued-send cleanup".into()));
    }
    let joined = worker.join();
    joined.unwrap();
    assert!(matches!(result.unwrap(), Err(Refusal::Os { why, .. }) if why.contains("absolute")));
    assert!(matches!(request.try_recv(), Err(mpsc::TryRecvError::Empty)));
    let state = client.conn.shared.state().unwrap();
    assert!(state.calls.is_empty() && state.programs.is_empty() && state.given.is_empty());
    assert!(state.closed.is_none());
    assert!(
        !client
            .conn
            .shared
            .transport
            .0
            .closed
            .load(Ordering::Acquire)
    );
}

/// The refusals each deadline test makes, every one held to the bound: three, so std's condition variable, whose
/// refusal comes inside twice a loaded host's slowest precise wait some of the time, passes only if all three do.
#[cfg(target_os = "macos")]
const REFUSALS: usize = 3;
/// The precise waits before a deadline test's first refusal and after each: six, twenty-four in all. A refusal the host's
/// load delays past twice the slowest of them needs every one of them to have come quickly: at the load ROOT's red ran
/// under (load 18.5 on 18 cores, where 5 of 15 refusals came more than 5.5 ms late) that is (2/3)^24, 6e-5, so a run of
/// three refusals fails that way under one time in ten thousand.
#[cfg(target_os = "macos")]
const CONTROLS: usize = 6;

/// How late a precise wait comes past its deadline on this thread now: poll(2) of no descriptors to a deadline `CAP`
/// from now. XNU sleeps it with the kernel's own leeway (an eighth of the wait, at most 1 ms, twice that while the user
/// is idle) whatever the thread's QoS, and it is none of hex-wait's (docs/design/deadline-waits.md), so what it comes
/// late by is the host's own: its timer, and the thread's wait for a core under the load the suites run at.
#[cfg(target_os = "macos")]
fn precise_lateness() -> Duration {
    let deadline = Instant::now().checked_add(CAP).unwrap();
    loop {
        let left = deadline.saturating_duration_since(Instant::now());
        if left.is_zero() {
            return Instant::now().saturating_duration_since(deadline);
        }
        // A signal ends it early: the loop waits out what is left.
        let timeout = rustix::event::Timespec::try_from(left).unwrap();
        let _ = rustix::event::poll(&mut [], Some(&timeout));
    }
}

/// `refusal`, a refusal at a deadline (what it gave, and how late past its deadline it came), made [`REFUSALS`] times on
/// this thread between precise waits: [`CONTROLS`] before the first and after each, each taught to `precise`, so every
/// refusal has the host's own lateness measured on both sides of it, in the same run and thread.
#[cfg(target_os = "macos")]
fn beside_precise_waits<T>(
    precise: &Patience,
    mut refusal: impl FnMut() -> (T, Duration),
) -> Vec<(T, Duration)> {
    let teach = || {
        for _ in 0..CONTROLS {
            hex_testkit::waits::answered(precise, precise_lateness());
        }
    };
    teach();
    let mut refusals = Vec::new();
    for _ in 0..REFUSALS {
        refusals.push(refusal());
        teach();
    }
    refusals
}

/// The wait the deadline tests hold each refusal to: twice the slowest precise wait recorded or seen, as every test
/// wait's patience is (hex_core::patience), seeded from `waits::OVERSHOOT` (the NOTE_CRITICAL timer's slowest of 1,000
/// waits on idle Macs) and taught by the run's own precise waits ([`beside_precise_waits`]), so the bound rises with the
/// load the suites run at, never past what std's condition variable comes to at utility QoS, four times the wait and
/// 75 ms. The census gets each precise wait, named `test.hex_client.budget.precise_late`: the record of a precise wait
/// under the suites' load.
#[cfg(target_os = "macos")]
fn precise_patience() -> Patience {
    Patience::seeded(
        "test.hex_client.budget.precise_late",
        hex_testkit::waits::OVERSHOOT,
    )
}

/// AC-T7.4.1: do queue actual deletes behind the held sender, each with an absolute deadline, between precise waits on
/// the same thread; expect each refused no later than its deadline and twice the slowest precise wait recorded or seen
/// ([`precise_patience`]). The deadline is the claim under test. macOS times std's condition variable waits by the
/// waiting thread's QoS: at utility QoS a queued sender capped at 25 ms was refused at 95.1 and 98.6 ms (the waits census
/// on 75694786, at cb166de1).
#[cfg(target_os = "macos")]
#[test]
fn ac_t7_4_1_a_queued_sender_is_refused_by_its_deadline() {
    /// The queued deletes' refusals at their absolute deadlines, the sender held, and the precise waits beside them.
    static CAPPED: Patience = Patience::measuring("test.hex_client.budget.queued_delete_deadline");
    let (transport, request) = stalled(None);
    let client = Arc::new(configured(AbsoluteStalled(transport)));
    let whole = client.conn.shared.sending.lock(None).unwrap().unwrap();
    let control = Arc::clone(&client);
    let (finished, answer) = mpsc::sync_channel(1);
    let worker = std::thread::spawn(move || {
        let precise = precise_patience();
        let refusals = beside_precise_waits(&precise, || {
            let deadline = Instant::now().checked_add(CAP).unwrap();
            let refused = control.delete_before(&BoxId::new("deadline-control").unwrap(), deadline);
            (refused, Instant::now().saturating_duration_since(deadline))
        });
        let _ = finished.send((refusals, precise.deadline()));
    });
    let result = recv_on(&CAPPED, &answer);
    drop(whole);
    if result.is_err() {
        client
            .conn
            .shared
            .end(Refusal::Broken("deadline queued-send cleanup".into()));
    }
    worker.join().unwrap();
    let (refusals, bound) = result.unwrap();
    for (refused, late) in refusals {
        assert!(matches!(refused, Err(Refusal::Os { why, .. }) if why.contains("absolute")));
        // None while the waits census measures: each lateness is recorded, held to no bound.
        if let Some(bound) = bound {
            assert!(
                late <= bound,
                "refused {late:?} past its deadline, past twice the slowest precise wait recorded or seen, {bound:?}"
            );
        }
    }
    assert!(matches!(request.try_recv(), Err(mpsc::TryRecvError::Empty)));
}

/// AC-T7.4.1: do send actual deletes, each with an absolute deadline, while another call holds the reader's turn, so
/// each waits for its answer on its mailbox, between precise waits on the same thread; expect each refused no later than
/// its deadline and twice the slowest precise wait recorded or seen ([`precise_patience`]). The deadline is the claim
/// under test. macOS times std's channel receive, a dispatch semaphore's wait, by the waiting thread's QoS, as it times
/// its condition variable.
#[cfg(target_os = "macos")]
#[test]
fn ac_t7_4_1_a_call_waiting_for_its_answer_is_refused_by_its_deadline() {
    /// The deletes' refusals at their absolute deadlines, their answers waited for on their mailboxes, and the precise
    /// waits beside them.
    static CAPPED: Patience = Patience::measuring("test.hex_client.budget.mailbox_deadline");
    let (transport, request) = stalled(None);
    let client = Arc::new(configured(AbsoluteStalled(transport)));
    // Held, as another call's would be: no call here can take the turn, so each waits on its mailbox.
    client.conn.shared.state().unwrap().reading = true;
    let control = Arc::clone(&client);
    let (finished, answer) = mpsc::sync_channel(1);
    let worker = std::thread::spawn(move || {
        let precise = precise_patience();
        let refusals = beside_precise_waits(&precise, || {
            let deadline = Instant::now().checked_add(CAP).unwrap();
            let refused = control.delete_before(&BoxId::new("mailbox-control").unwrap(), deadline);
            (refused, Instant::now().saturating_duration_since(deadline))
        });
        let _ = finished.send((refusals, precise.deadline()));
    });
    let result = recv_on(&CAPPED, &answer);
    if result.is_err() {
        client
            .conn
            .shared
            .end(Refusal::Broken("mailbox deadline cleanup".into()));
    }
    worker.join().unwrap();
    let (refusals, bound) = result.unwrap();
    for (refused, late) in refusals {
        assert!(matches!(refused, Err(Refusal::Os { why, .. }) if why.contains("absolute")));
        // None while the waits census measures: each lateness is recorded, held to no bound.
        if let Some(bound) = bound {
            assert!(
                late <= bound,
                "refused {late:?} past its deadline, past twice the slowest precise wait recorded or seen, {bound:?}"
            );
        }
    }
    // Its requests went, and only their answers were waited for.
    assert!(matches!(request.try_recv(), Ok(())));
    assert!(client.conn.shared.state().unwrap().calls.is_empty());
}

/// AC-T7.4.1: supplied instants survive unchanged; an earlier transport bound may shorten
/// authority, no later/default bound can enlarge it, and a learned wait never replaces it:
/// ending first it is a run-out to record, a tie the caller's; with no caller it is the bound.
#[test]
fn absolute_caller_limit_keeps_original_instant_and_smaller_bounds() {
    let began = Instant::now();
    let at = began.checked_add(LONG_CAP).unwrap();
    let absolute = ReplyDeadline::caller_before(at).unwrap();
    assert_eq!(absolute.at(), at);
    assert_eq!(absolute.limited_by(None).at(), at);
    let earlier = began.checked_add(CAP).unwrap();
    let configured = ReplyDeadline {
        at: earlier,
        source: ReplyBound::Caller(CAP),
    };
    let limited = absolute.limited_by(Some(configured));
    assert_eq!(limited.at(), earlier);
    let later = ReplyDeadline {
        at,
        source: ReplyBound::Caller(LONG_CAP),
    };
    assert_eq!(limited.limited_by(Some(later)).at(), earlier);
    let tied = absolute.limited_by(Some(later));
    assert!(matches!(tied.source, ReplyBound::CallerAbsolute));
    let (bound, learned) = ReplyDeadline::combine(Some(absolute), began, None);
    assert_eq!(bound.unwrap().at(), at);
    assert!(learned.is_none());
    let (bound, learned) = ReplyDeadline::combine(Some(absolute), began, Some(CAP));
    assert_eq!(bound.unwrap().at(), at);
    let learned = learned.unwrap();
    assert_eq!(learned.at(), earlier);
    assert!(matches!(learned.source, ReplyBound::Learned(_)));
    let (bound, learned) = ReplyDeadline::combine(Some(absolute), began, Some(LONG_CAP));
    assert_eq!(bound.unwrap().at(), at);
    assert!(learned.is_none(), "a tie belongs to caller authority");
    let (bound, learned) = ReplyDeadline::combine(None, began, Some(CAP));
    let bound = bound.unwrap();
    assert_eq!(bound.at(), earlier);
    assert!(matches!(bound.source, ReplyBound::Learned(_)));
    assert!(learned.is_none());
}

/// Shared observations of each actual port and its original optional dial deadline.
type DialDeadlines = Arc<Mutex<Vec<(u32, Option<Instant>)>>>;

struct AbsoluteNamedLink {
    control: Mutex<Option<TcpStream>>,
    stdio: Mutex<VecDeque<TcpStream>>,
    deadlines: DialDeadlines,
}

impl Link for AbsoluteNamedLink {
    fn dial(&self, port: u32) -> Result<DuplexOwner, Refusal> {
        self.dial_before(port, None)
    }
    fn dial_before(&self, port: u32, deadline: Option<Instant>) -> Result<DuplexOwner, Refusal> {
        self.deadlines.lock().unwrap().push((port, deadline));
        if deadline.is_some_and(|at| at <= Instant::now()) {
            return Err(Refusal::os(
                "absolute fixture dial",
                io::ErrorKind::TimedOut,
            ));
        }
        let stream = if port == hex_core::stream::CONTROL_PORT {
            self.control.lock().unwrap().take()
        } else {
            self.stdio.lock().unwrap().pop_front()
        };
        stream
            .ok_or_else(|| Refusal::Broken("owned absolute fixture stream absent".into()))
            .and_then(own_duplex)
    }
}

fn absolute_pair() -> (TcpStream, TcpStream) {
    /// A loopback connection, connected and accepted.
    static PAIRED: Patience = Patience::measuring("test.hex_client.budget.loopback_paired");
    let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
    listener.set_nonblocking(true).unwrap();
    let address = listener.local_addr().unwrap();
    let (client, peer) = within(&PAIRED, |by| -> io::Result<_> {
        let client = match timeout_by(by)? {
            Some(left) => TcpStream::connect_timeout(&address, left)?,
            None => TcpStream::connect(address)?,
        };
        let peer = accept_before(&listener, &AtomicBool::new(false), &Wake::new()?, by)?;
        Ok((client, peer))
    })
    .unwrap();
    client.set_nodelay(true).unwrap();
    peer.set_nodelay(true).unwrap();
    (client, peer)
}

/// AC-T7.4.1: actual guest Stream run setup with no Link default writes all three
/// native named headers under one caller cap. No server answers the Run, so the
/// same cap must also bound its unarmed control answer and close the asked ends.
#[test]
fn absolute_run_caps_named_stdio_and_preserves_headers() {
    use hex_core::stream::{self, Named};
    /// The run's refusal at its caller's cap, no answer come.
    static CAPPED: Patience = Patience::measuring("test.hex_client.budget.absolute_run_capped");
    /// The three named streams' headers, each stream then closed by the run.
    static NAMED: Patience = Patience::measuring("test.hex_client.budget.absolute_run_named");
    /// The run's request, come to the agent's end of the control.
    static ASKED: Patience = Patience::measuring("test.hex_client.budget.absolute_run_asked");
    let (control, mut control_peer) = absolute_pair();
    control_peer.write_all(&11u64.to_le_bytes()).unwrap();
    let mut named = VecDeque::new();
    let mut peers = Vec::new();
    for _ in 0..3 {
        let (stream, peer) = absolute_pair();
        named.push_back(stream);
        peers.push(peer);
    }
    let deadlines = Arc::new(Mutex::new(Vec::new()));
    let link = guest::own_link(AbsoluteNamedLink {
        control: Mutex::new(Some(control)),
        stdio: Mutex::new(named),
        deadlines: Arc::clone(&deadlines),
    })
    .unwrap();
    let transport = guest::Stream::dial(link).unwrap();
    let client = Arc::new(configured(transport));
    let control = Arc::clone(&client);
    let (begun, began) = mpsc::sync_channel(1);
    let (finished, answer) = mpsc::sync_channel(1);
    let worker = std::thread::spawn(move || {
        let deadline = Instant::now().checked_add(CAP).unwrap();
        let _ = begun.send(deadline);
        let result = control
            .run_before(
                absolute_spec(),
                Program::of(&["unused"]),
                Stdio {
                    ask: hex_api::Ask {
                        stdin: true,
                        stdout: true,
                        stderr: true,
                    },
                    ..Stdio::null()
                },
                deadline,
            )
            .map(drop);
        let _ = finished.send(result);
    });
    // Its deadline's notice precedes its answer, so it is read once the answer has come.
    let result = recv_on(&CAPPED, &answer);
    let original = began.try_recv();
    if result.is_err() {
        client
            .conn
            .shared
            .end(Refusal::Broken("absolute named observation cleanup".into()));
        let _ = control_peer.shutdown(Shutdown::Both);
        for peer in &peers {
            let _ = peer.shutdown(Shutdown::Both);
        }
    }
    let joined = worker.join();
    // Each read below waits on its own fact's patience, after the actor joined: what the run wrote before it ended.
    let observed = within(&NAMED, |by| -> io::Result<Vec<Named>> {
        let mut names = Vec::new();
        for peer in &mut peers {
            peer.set_read_timeout(timeout_by(by)?)?;
            let mut name = [0; stream::NAME_BYTES];
            peer.read_exact(&mut name)?;
            names.push(
                stream::read_name(&name).map_err(|error| io::Error::other(format!("{error:?}")))?,
            );
            if peer.read(&mut [0])? != 0 {
                return Err(io::Error::other("unasked payload"));
            }
        }
        Ok(names)
    });
    let request_seen = within(&ASKED, |by| {
        control_peer.set_read_timeout(timeout_by(by)?)?;
        next_request(&mut control_peer)
    });
    let state_clean = {
        let state = client.conn.shared.state().unwrap();
        state.calls.is_empty()
            && state.programs.is_empty()
            && state.given.is_empty()
            && state.closed.is_none()
    };
    let recorded = deadlines.lock().unwrap().clone();
    let _ = control_peer.shutdown(Shutdown::Both);
    for peer in &peers {
        let _ = peer.shutdown(Shutdown::Both);
    }
    joined.unwrap();
    let original = original.unwrap();
    eprintln!(
        "client_absolute named_deadlines={recorded:?} caller={original:?} result={result:?} observed={observed:?} request={request_seen:?}"
    );
    assert!(matches!(result.unwrap(), Err(Refusal::Os { why, .. }) if why.contains("caller")));
    let names = observed.unwrap();
    let (request_header, request) = request_seen.unwrap();
    assert!(matches!(
        request,
        Request::Call(Call::Run {
            streams: Streams {
                stdin: true,
                stdout: true,
                stderr: true
            },
            ..
        })
    ));
    assert_eq!(recorded.len(), 4);
    assert_eq!(recorded[0], (stream::CONTROL_PORT, None));
    let setup = recorded[1].1.unwrap();
    assert!(
        setup <= original
            && recorded[1..]
                .iter()
                .all(|(port, at)| *port == stream::STDIO_PORT && *at == Some(setup))
    );
    assert_eq!(names.len(), 3);
    assert!(
        names
            .iter()
            .all(|name| name.exec.conn == 11 && name.exec.id == request_header.id)
    );
    assert_eq!(names[0].stream, stream::Stdio::In);
    assert_eq!(names[1].stream, stream::Stdio::Out);
    assert_eq!(names[2].stream, stream::Stdio::Err);
    assert!(state_clean);
}

/// The existing in-memory offer fixture supplies an actual Started response,
/// while no Exit is ever published. It has no relative transport reply cap.
struct DropOriginal(Offers);

impl Transport for DropOriginal {
    type Reading = ();

    fn answers() -> &'static [Patience; KINDS] {
        static WAITS: [Patience; KINDS] =
            [const { Patience::measuring("test.client_drop_original.unarmed") }; KINDS];
        &WAITS
    }
    fn reading(&self) {}
    fn handle(&self) -> Handle<'_> {
        self.0.handle()
    }
    fn recv(&self, reading: &mut (), take: &mut Take<'_>) -> Result<bool, Refusal> {
        self.0.recv(reading, take)
    }
    fn try_send(&self, bytes: &[u8], at: &mut usize, lent: &[Lent<'_>]) -> Result<bool, Sent> {
        let (header, request) = wire::read_request_apart(bytes, wire::MAX_MESSAGE).unwrap();
        if matches!(&request, Request::Call(Call::Run { .. })) {
            self.0.queue(header.id, Answer::Started { pid: 7 });
            self.0.outbound.lock().unwrap().push((header.id, request));
            *at = bytes.len();
            Ok(true)
        } else {
            self.0.try_send(bytes, at, lent)
        }
    }
    fn shutdown(&self) {
        self.0.shutdown();
    }
}

/// AC-T7.4.1 original regression: after run_before's absolute cap expires,
/// returning an observer error must not renew authority while dropping its actual
/// unfinished RemoteProc. Only the existing sender guard delays that Drop here.
#[test]
fn absolute_run_error_return_does_not_renew_drop_sender_budget() {
    /// The observer's error return, its unfinished Proc dropped with the sender held.
    static RETURNED: Patience = Patience::measuring("test.hex_client.budget.drop_returned");
    let (io, _) = stalled(None);
    let client = Arc::new(configured(DropOriginal(Offers::unobserved(io))));
    let original = Instant::now().checked_add(CAP).unwrap();
    let mut process = client
        .run_before(
            absolute_spec(),
            Program::of(&["owned-drop-original"]),
            Stdio::null(),
            original,
        )
        .unwrap();
    let run_id = {
        let sent = client.conn.shared.transport.0.outbound.lock().unwrap();
        assert_eq!(sent.len(), 1);
        let (id, request) = &sent[0];
        assert!(matches!(request, Request::Call(Call::Run { .. })));
        *id
    };
    assert_eq!(process.pid(), 7);
    assert_eq!(process.try_wait().unwrap(), None);
    assert_eq!(client.conn.shared.state().unwrap().programs.len(), 1);
    assert_eq!(client.conn.shared.transport.reply_budget(), None);

    let (release, begin) = mpsc::sync_channel(1);
    let (starting, started) = mpsc::sync_channel(1);
    let (finished, answer) = mpsc::sync_channel(1);
    // Spawn before holding the sender guard: even spawn failure drops the
    // captured Proc without a self-owned guard that could strand its destructor.
    let worker = std::thread::Builder::new()
        .name("owned-drop-original".into())
        .spawn(move || {
            let permitted = begin.recv();
            let began = Instant::now();
            let _ = starting.send((began, permitted.is_ok()));
            let fail_observer = move || -> Result<(), Refusal> {
                let _unfinished = process;
                Err(Refusal::os(
                    "owned observer original-deadline failure",
                    io::ErrorKind::TimedOut,
                ))
            };
            // This return must run RemoteProc::drop before the result can be sent.
            let result = fail_observer();
            let _ = finished.send((Instant::now(), result));
        })
        .unwrap();

    // From this point every observation is captured; cleanup/join precedes ALL
    // assertions. No production hook, fake clock, new timer or periodic wait.
    let held = client.conn.shared.sending.lock(None);
    let held_exact_guard = matches!(&held, Ok(Some(_)));
    let expired = until(original);
    let released = release.send(());
    // Its notice that it began precedes its answer, so it is read once the answer has come.
    let before_cleanup = recv_on(&RETURNED, &answer);
    let began = started.try_recv();
    let before_release_at = Instant::now();
    drop(held);
    client.conn.shared.end(Refusal::Broken(
        "owned Drop original observation cleanup".into(),
    ));
    let joined = worker.join();
    let after_cleanup = answer.try_recv();
    let (release_count, other_count) = {
        let sent = client.conn.shared.transport.0.outbound.lock().unwrap();
        let releases = sent
            .iter()
            .filter(|(_, request)| matches!(request, Request::Release))
            .count();
        let other = sent
            .iter()
            .filter(|(_, request)| {
                !matches!(request, Request::Call(Call::Run { .. }) | Request::Release)
            })
            .count();
        (releases, other)
    };
    let clean =
        client.conn.shared.state().map(|state| {
            state.calls.is_empty() && state.programs.is_empty() && state.given.is_empty()
        });
    eprintln!(
        "client_drop_original run_id={run_id} original={original:?} cap={CAP:?} held={held_exact_guard} expired={expired:?} gate={released:?} began={began:?} before_release={before_release_at:?} before_cleanup={before_cleanup:?} after_cleanup={after_cleanup:?} release_count={release_count} other_count={other_count} joined={} clean={clean:?}",
        joined.is_ok()
    );
    joined.unwrap();
    expired.unwrap();
    released.unwrap();
    assert!(held_exact_guard, "the exact existing sender must be held");
    let (began, permitted) = began.unwrap();
    assert!(
        permitted && began >= original,
        "Drop must begin after the original cap"
    );
    assert_eq!(other_count, 0);
    assert!(clean.unwrap());
    assert!(
        client
            .conn
            .shared
            .transport
            .0
            .io
            .closed
            .load(Ordering::Acquire)
    );
    assert!(
        before_cleanup.is_ok(),
        "original absolute deadline was lost: the observer's final error return waited in unfinished Proc::drop until its held sender was released"
    );
    let (returned, result) = before_cleanup.unwrap();
    assert!(returned <= before_release_at);
    assert!(matches!(
        result,
        Err(Refusal::Os {
            op: "owned observer original-deadline failure",
            ..
        })
    ));
}

struct BootstrapProbe {
    io: Offers,
    reply: Mutex<Option<Answer>>,
}

impl Transport for BootstrapProbe {
    type Reading = ();

    fn answers() -> &'static [Patience; KINDS] {
        static WAITS: [Patience; KINDS] =
            [const { Patience::measuring("test.client_bootstrap.unarmed") }; KINDS];
        &WAITS
    }

    fn reading(&self) {}
    fn handle(&self) -> Handle<'_> {
        self.io.handle()
    }
    fn recv(&self, reading: &mut (), take: &mut Take<'_>) -> Result<bool, Refusal> {
        self.io.recv(reading, take)
    }
    fn try_send(&self, bytes: &[u8], at: &mut usize, lent: &[Lent<'_>]) -> Result<bool, Sent> {
        let (header, request) = wire::read_request_apart(bytes, wire::MAX_MESSAGE).unwrap();
        self.io.outbound.lock().unwrap().push((header.id, request));
        if let Some(answer) = self.reply.lock().unwrap().take() {
            self.io.queue(header.id, answer);
        }
        self.io.io.try_send(bytes, at, lent)
    }
    fn shutdown(&self) {
        self.io.shutdown();
    }
}

fn bootstrap_probe(reply: Option<Answer>) -> (Core<BootstrapProbe>, mpsc::Receiver<()>) {
    let (io, sent) = stalled(None);
    (
        configured(BootstrapProbe {
            io: Offers::unobserved(io),
            reply: Mutex::new(reply),
        }),
        sent,
    )
}

#[derive(Clone, Copy, Debug)]
enum BootstrapCall {
    Capacity,
    PhysicalReport,
    Create,
}

impl BootstrapCall {
    fn invoke(self, client: &Core<BootstrapProbe>, at: Instant) -> Result<Answer, Refusal> {
        match self {
            Self::Capacity => client.capacity_before(Class::Oci, at).map(Answer::Capacity),
            Self::PhysicalReport => client
                .physical_report_before(at)
                .map(Answer::PhysicalReport),
            Self::Create => client.create_before(absolute_spec(), at).map(Answer::Info),
        }
    }

    fn reply(self) -> Answer {
        match self {
            Self::Capacity => Answer::Capacity(None),
            Self::PhysicalReport => Answer::PhysicalReport(hex_api::PhysicalReport {
                unavailable: Vec::new(),
                authority: "owned-bootstrap-stand-in".into(),
                devices: Vec::new(),
            }),
            Self::Create => {
                let spec = absolute_spec();
                Answer::Info(BoxInfo {
                    published: Vec::new(),
                    grants: Vec::new(),
                    id: spec.id,
                    class: Class::Process,
                    state: hex_api::BoxState::Empty,
                    bounds: spec.bounds,
                    allocation: None,
                    adopted: false,
                    membership: None,
                    exit: None,
                    abandoned: 0,
                    route: None,
                    last_resort: None,
                })
            }
        }
    }

    fn matches_request(self, request: &Request) -> bool {
        match (self, request) {
            (Self::Capacity, Request::Call(Call::Capacity(Class::Oci)))
            | (Self::PhysicalReport, Request::Call(Call::PhysicalReport)) => true,
            (Self::Create, Request::Call(Call::Create(spec))) => **spec == absolute_spec(),
            _ => false,
        }
    }
}

/// AC-T7.4.1: all three bootstrap wrappers retain typed request/answer/refusal
/// behavior and reject an already expired original deadline before any send.
#[test]
fn absolute_bootstrap_preserves_requests_and_typed_answers() {
    /// A bootstrap call's answer from the probe, which answers as it is asked.
    static ANSWERED: Patience = Patience::measuring("test.hex_client.budget.bootstrap_answered");
    for kind in [
        BootstrapCall::Capacity,
        BootstrapCall::PhysicalReport,
        BootstrapCall::Create,
    ] {
        let (client, sent) = bootstrap_probe(None);
        let expired = kind.invoke(&client, Instant::now());
        assert!(matches!(expired, Err(Refusal::Os { .. })));
        assert!(matches!(sent.try_recv(), Err(mpsc::TryRecvError::Empty)));
        assert!(
            client
                .conn
                .shared
                .transport
                .io
                .outbound
                .lock()
                .unwrap()
                .is_empty()
        );

        let reply = kind.reply();
        let expected = wire::answer(0, &reply, wire::MAX_MESSAGE).unwrap();
        let (client, _) = bootstrap_probe(Some(reply));
        let got = within(&ANSWERED, |by| kind.invoke(&client, answer_by(by))).unwrap();
        assert_eq!(wire::answer(0, &got, wire::MAX_MESSAGE).unwrap(), expected);
        let outbound = client.conn.shared.transport.io.outbound.lock().unwrap();
        assert_eq!(outbound.len(), 1);
        assert!(kind.matches_request(&outbound[0].1));
        assert!(client.conn.shared.state().unwrap().programs.is_empty());
        drop(outbound);

        let (client, _) = bootstrap_probe(Some(Answer::Done));
        assert!(matches!(
            within(&ANSWERED, |by| kind.invoke(&client, answer_by(by))),
            Err(Refusal::Broken(_))
        ));

        let why = Refusal::Broken("owned typed bootstrap refusal".into());
        let (client, _) = bootstrap_probe(Some(Answer::Refused(why.clone())));
        assert_eq!(
            within(&ANSWERED, |by| kind.invoke(&client, answer_by(by))),
            Err(why)
        );
    }
}

/// AC-T7.4.1: even an unarmed answer kind and absent relative transport budget
/// cannot extend the original bootstrap authority. Cleanup/join precedes assertions.
#[test]
fn absolute_bootstrap_caps_unarmed_answer_waits() {
    /// The bootstrap call's refusal at its absolute cap, no answer come.
    static CAPPED: Patience = Patience::measuring("test.hex_client.budget.bootstrap_capped");
    for kind in [
        BootstrapCall::Capacity,
        BootstrapCall::PhysicalReport,
        BootstrapCall::Create,
    ] {
        let (client, sent) = bootstrap_probe(None);
        let client = Arc::new(client);
        let control = Arc::clone(&client);
        let at = Instant::now().checked_add(CAP).unwrap();
        let (finished, answer) = mpsc::sync_channel(1);
        let worker = std::thread::spawn(move || {
            let result = kind.invoke(&control, at);
            let _ = finished.send(result);
        });
        // Its send precedes its answer, so it is read once the answer has come.
        let observed = recv_on(&CAPPED, &answer);
        let observed_send = sent.try_recv();
        client.conn.shared.end(Refusal::Broken(
            "owned bootstrap observation cleanup".into(),
        ));
        let joined = worker.join();
        let outbound = client.conn.shared.transport.io.outbound.lock().unwrap();
        let clean = client.conn.shared.state().map(|state| {
            state.calls.is_empty() && state.programs.is_empty() && state.given.is_empty()
        });
        eprintln!(
            "client_bootstrap_absolute kind={kind:?} at={at:?} send={observed_send:?} observed={observed:?} joined={} clean={clean:?}",
            joined.is_ok()
        );
        joined.unwrap();
        observed_send.unwrap();
        assert_eq!(outbound.len(), 1);
        assert!(kind.matches_request(&outbound[0].1));
        assert!(clean.unwrap());
        assert!(matches!(
            observed.unwrap(),
            Err(Refusal::Os { why, .. }) if why.contains("absolute") && !why.contains("twice")
        ));
    }
}

struct SettleOriginal {
    io: Offers,
    setup: Mutex<Option<(u64, hex_api::Ask, ReplyDeadline)>>,
    started: mpsc::SyncSender<(u64, Instant)>,
    kill: mpsc::SyncSender<(u64, u64, Instant)>,
    exits: AtomicUsize,
    /// The agent's answer to the Kill, given as it comes; none withholds it.
    kill_answer: Mutex<Option<Answer>>,
    /// None leaves run_before's explicit cap alone; run and exec, which have no cap of their
    /// own, need one for the deadline-aware stream setup.
    budget: Option<Duration>,
}

impl Transport for SettleOriginal {
    type Reading = ();
    const MAKES_ASKED: bool = true;

    fn answers() -> &'static [Patience; KINDS] {
        static WAITS: [Patience; KINDS] =
            [const { Patience::measuring("test.client_settle_original.unarmed") }; KINDS];
        &WAITS
    }

    fn reply_budget(&self) -> Option<Duration> {
        self.budget
    }

    fn reading(&self) {}
    fn handle(&self) -> Handle<'_> {
        self.io.handle()
    }

    fn recv(&self, reading: &mut (), take: &mut Take<'_>) -> Result<bool, Refusal> {
        let mut observed = |message: &Received<'_>, fds| {
            let (header, answer) =
                wire::read_answer(message, wire::MAX_MESSAGE).map_err(Refusal::from)?;
            take(message, fds)?;
            match answer {
                Answer::Started { .. } => {
                    eprintln!("client_settle_phase phase=started id={}", header.id);
                    let _ = self.started.try_send((header.id, Instant::now()));
                }
                Answer::Exited(_) => {
                    self.exits.fetch_add(1, Ordering::SeqCst);
                }
                _ => {}
            }
            Ok(())
        };
        let got = self.io.recv(reading, &mut observed)?;
        // The wake is only a datagram, not persistent EOF. Drain actual queued
        // answers first, then preserve the same terminal condition as Stalled.
        if self.io.io.closed.load(Ordering::Acquire) {
            eprintln!("client_settle_phase phase=transport_closed");
            Err(Refusal::Broken("owned settle transport closed".into()))
        } else {
            Ok(got)
        }
    }

    fn try_send(&self, bytes: &[u8], at: &mut usize, _: &[Lent<'_>]) -> Result<bool, Sent> {
        let (header, request) = wire::read_request_apart(bytes, wire::MAX_MESSAGE).unwrap();
        eprintln!(
            "client_settle_phase phase=send kind={:#06x} id={}",
            header.kind, header.id
        );
        match &request {
            Request::Call(Call::Run { .. } | Call::Exec { .. }) => {
                self.io.queue(header.id, Answer::Started { pid: 7 })
            }
            Request::Call(Call::Kill { exec }) => {
                eprintln!("client_settle_phase phase=kill_request exec={exec}");
                // The real Kill reached the sender after wait(None). Answer it as the test
                // says, or withhold only its answer.
                let _ = self.kill.try_send((header.id, *exec, Instant::now()));
                if let Some(answer) = self.kill_answer.lock().unwrap().take() {
                    self.io.queue(header.id, answer);
                }
            }
            Request::Release => {}
            _ => {
                return Err(Sent::Refused(Refusal::Broken(
                    "unexpected owned settle-original request".into(),
                )));
            }
        }
        self.io.outbound.lock().unwrap().push((header.id, request));
        *at = bytes.len();
        Ok(true)
    }

    fn asks_before(
        &self,
        exec: u64,
        ask: hex_api::Ask,
        deadline: ReplyDeadline,
    ) -> Result<hex_api::Given, Refusal> {
        deadline.remaining("owned malformed stream setup")?;
        *self.setup.lock().unwrap() = Some((exec, ask, deadline));
        // Deliberately return the wrong end: the caller asks for stdin only. This
        // bounded adapter fault is refused by real settle AFTER actual Started.
        let mut program = Stdio {
            ask: hex_api::Ask {
                stdout: true,
                ..hex_api::Ask::default()
            },
            ..Stdio::null()
        };
        let given = program.make_asked()?;
        *self.io.program.lock().unwrap() = Some(program);
        eprintln!(
            "client_settle_phase phase=setup_ends_returned exec={exec} deadline={:?}",
            deadline.at()
        );
        Ok(given)
    }

    fn carries_before(&self, _: u64, stdio: Stdio, deadline: ReplyDeadline) -> Result<(), Refusal> {
        deadline.remaining("owned null carried streams")?;
        stdio.check()?;
        if carried(&stdio) != Streams::default() {
            return Err(Refusal::Broken("unexpected owned carried pipe".into()));
        }
        Ok(())
    }

    fn shutdown(&self) {
        self.io.shutdown();
    }
}

/// AC-T7.4.1 original: an actual Started followed by a bounded stream-settlement
/// refusal must not replace run_before's authority with a fresh Kill answer wait.
#[test]
fn absolute_run_settle_error_does_not_renew_kill_answer_budget() {
    /// The run's return, its settle refused and its Kill unanswered within its original authority.
    static RETURNED: Patience =
        Patience::measuring("test.hex_client.budget.settle_original_returned");
    let (io, _) = stalled(None);
    let (starting, started) = mpsc::sync_channel(1);
    let (killing, killed) = mpsc::sync_channel(1);
    let client = Arc::new(configured(SettleOriginal {
        io: Offers::unobserved(io),
        setup: Mutex::new(None),
        started: starting,
        kill: killing,
        exits: AtomicUsize::new(0),
        kill_answer: Mutex::new(None),
        budget: None,
    }));
    let control = Arc::clone(&client);
    let original = Instant::now().checked_add(CAP).unwrap();
    let (finished, answer) = mpsc::sync_channel(1);
    let worker = std::thread::Builder::new()
        .name("owned-settle-original".into())
        .spawn(move || {
            eprintln!("client_settle_phase phase=run_begin original={original:?}");
            let result = control
                .run_before(
                    absolute_spec(),
                    Program::of(&["owned-settle-original"]),
                    Stdio {
                        ask: hex_api::Ask {
                            stdin: true,
                            ..hex_api::Ask::default()
                        },
                        ..Stdio::null()
                    },
                    original,
                )
                .map(drop);
            eprintln!("client_settle_phase phase=worker_return result={result:?}");
            let _ = finished.send((Instant::now(), result));
        })
        .unwrap();

    // Capture every error branch; end ONLY this connection, close its retained
    // pipe, and join this worker BEFORE asserting any result or phase witness. Its Started
    // and its Kill precede its answer, so each is read once the answer has come.
    let before_cleanup = recv_on(&RETURNED, &answer);
    let actual_started = started.try_recv();
    let actual_kill = killed.try_recv();
    let expired = until(original);
    let cleanup_at = Instant::now();
    eprintln!(
        "client_settle_phase phase=cleanup_begin started={actual_started:?} kill={actual_kill:?} before={before_cleanup:?}"
    );
    client
        .conn
        .shared
        .end(Refusal::Broken("owned settle-original cleanup".into()));
    eprintln!("client_settle_phase phase=end_returned");
    let (pipe, pipe_poisoned) = match client.conn.shared.transport.io.program.lock() {
        Ok(mut program) => (program.take(), false),
        // Cleanup may recover only this test-owned guard so a poisoned fixture
        // cannot skip its pipe close/join. The poison remains a failed witness.
        Err(poisoned) => (poisoned.into_inner().take(), true),
    };
    let pipe_was_owned = pipe.is_some();
    drop(pipe);
    eprintln!("client_settle_phase phase=pipe_closed join=begin");
    let joined = worker.join();
    eprintln!("client_settle_phase phase=joined ok={}", joined.is_ok());
    let after_cleanup = answer.try_recv();
    let transport = &client.conn.shared.transport;
    let setup = *transport.setup.lock().unwrap();
    let exits = transport.exits.load(Ordering::SeqCst);
    let sent = transport.io.outbound.lock().unwrap();
    let run_ids: Vec<_> = sent
        .iter()
        .filter(|(_, request)| matches!(request, Request::Call(Call::Run { .. })))
        .map(|(id, _)| *id)
        .collect();
    let kills: Vec<_> = sent
        .iter()
        .filter_map(|(id, request)| match request {
            Request::Call(Call::Kill { exec }) => Some((*id, *exec)),
            _ => None,
        })
        .collect();
    let releases = sent
        .iter()
        .filter(|(_, request)| matches!(request, Request::Release))
        .count();
    let clean = client.conn.shared.state().map(|state| {
        state.calls.is_empty()
            && state.programs.is_empty()
            && state.given.is_empty()
            && state.waiting.is_empty()
    });
    eprintln!(
        "client_settle_original original={original:?} cap={CAP:?} setup={setup:?} started={actual_started:?} kill={actual_kill:?} runs={run_ids:?} kills={kills:?} exits={exits} releases={releases} before_cleanup={before_cleanup:?} cleanup_at={cleanup_at:?} after_cleanup={after_cleanup:?} pipe_owned={pipe_was_owned} pipe_poisoned={pipe_poisoned} joined={} clean={clean:?}",
        joined.is_ok()
    );
    joined.unwrap();
    expired.unwrap();
    assert!(pipe_was_owned);
    assert!(!pipe_poisoned);
    assert!(transport.io.io.closed.load(Ordering::Acquire));
    assert!(transport.io.program.lock().unwrap().is_none());
    assert!(clean.unwrap());
    assert_eq!(transport.reply_budget(), None);
    assert_eq!(run_ids.len(), 1);
    assert_eq!(exits, 0);
    let (exec, ask, cap) = setup.unwrap();
    assert_eq!(exec, run_ids[0]);
    assert!(ask.stdin && !ask.stdout && !ask.stderr);
    assert_eq!(cap.at(), original);
    assert_eq!(actual_started.unwrap().0, exec);
    if before_cleanup.is_err() {
        // In the original, this positive witness localizes the delay AFTER the
        // zero-duration wait(None) poll: it reached the real unarmed Kill call.
        let (request, target, _) = actual_kill.expect("missing Kill is an unverified phase");
        assert_eq!(target, exec);
        assert_eq!(kills, [(request, exec)]);
    }
    let returned_error = before_cleanup
        .as_ref()
        .ok()
        .or_else(|| after_cleanup.as_ref().ok());
    assert!(
        matches!(returned_error, Some((_, Err(Refusal::Lifecycle { id, cause: hex_api::LifecycleCause::Uncertain }))) if *id == absolute_spec().id),
        "a Kill unanswered within the original authority leaves the run's end uncertain"
    );
    assert!(
        before_cleanup.is_ok(),
        "original stream-settlement error return exceeded run_before authority while waiting for the ordinary Kill answer"
    );
    let (returned, _) = before_cleanup.unwrap();
    assert!(returned <= cleanup_at);
}

/// How a settle test starts its program: under run_before's explicit cap, whose failed setup
/// kills through kill_before, or by run or exec, which have none and kill through Proc::kill.
#[derive(Clone, Copy, Debug)]
enum SettleStart {
    RunBefore,
    Run,
    Exec,
}

/// What a settle test saw.
#[derive(Debug)]
struct Settled {
    start: SettleStart,
    result: Result<(), Refusal>,
    /// The box the start named.
    id: BoxId,
    /// The start's request id, which names its exec.
    starts: Vec<u64>,
    /// The execs the Kills named.
    kills: Vec<u64>,
    releases: usize,
    /// No call, program, given end or wait left registered.
    clean: bool,
}

/// A settle test's start returning, its settlement refused and its Kill answered as the agent says.
static SETTLED: Patience = Patience::measuring("test.hex_client.budget.settle_failure_returned");

/// Do start a program by `start` whose stream settlement fails after its actual Started (the
/// fixture gives an end its exec never asked for), the agent answering its Kill with `kill`, every
/// answer by `by`, a wait on [`SETTLED`].
fn settle_failure(start: SettleStart, kill: Answer, by: Option<Instant>) -> Settled {
    let (io, _) = stalled(None);
    let (starting, _) = mpsc::sync_channel(1);
    let (killing, _) = mpsc::sync_channel(1);
    let client = configured(SettleOriginal {
        io: Offers::unobserved(io),
        setup: Mutex::new(None),
        started: starting,
        kill: killing,
        exits: AtomicUsize::new(0),
        kill_answer: Mutex::new(Some(kill)),
        budget: match start {
            SettleStart::RunBefore => None,
            SettleStart::Run | SettleStart::Exec => {
                Some(answer_by(by).saturating_duration_since(Instant::now()))
            }
        },
    });
    let stdio = Stdio {
        ask: hex_api::Ask {
            stdin: true,
            ..hex_api::Ask::default()
        },
        ..Stdio::null()
    };
    let program = Program::of(&["owned-settle-kill"]);
    let spec = absolute_spec();
    let (id, result) = match start {
        SettleStart::RunBefore => (
            spec.id.clone(),
            client.run_before(spec, program, stdio, answer_by(by)),
        ),
        SettleStart::Run => (spec.id.clone(), client.run(spec, program, stdio)),
        SettleStart::Exec => {
            let id = BoxId::new("owned-settle-exec").unwrap();
            let result = client.exec(&id, program, stdio);
            (id, result)
        }
    };
    let result = result.map(drop);
    let shared = &client.conn.shared;
    let clean = shared
        .state()
        .map(|state| {
            state.calls.is_empty()
                && state.programs.is_empty()
                && state.given.is_empty()
                && state.waiting.is_empty()
        })
        .unwrap();
    let sent = shared.transport.io.outbound.lock().unwrap();
    Settled {
        start,
        result,
        id,
        starts: sent
            .iter()
            .filter(|(_, request)| {
                matches!(request, Request::Call(Call::Run { .. } | Call::Exec { .. }))
            })
            .map(|(id, _)| *id)
            .collect(),
        kills: sent
            .iter()
            .filter_map(|(_, request)| match request {
                Request::Call(Call::Kill { exec }) => Some(*exec),
                _ => None,
            })
            .collect(),
        releases: sent
            .iter()
            .filter(|(_, request)| matches!(request, Request::Release))
            .count(),
        clean,
    }
}

/// AC-T7.4.1: Do start a program by run_before, run and exec whose stream settlement fails after
/// its Started, the agent refusing each Kill; expect each start refused as its box's uncertain
/// lifecycle and no Release sent: the program may still run, and a Release lets it run on.
#[test]
fn a_settle_failure_whose_kill_is_refused_is_uncertain_and_never_released() {
    let seen: Vec<Settled> = [SettleStart::RunBefore, SettleStart::Run, SettleStart::Exec]
        .into_iter()
        .map(|start| {
            let refused = Refusal::os("killing the program", "the model agent refuses");
            within(&SETTLED, |by| {
                settle_failure(start, Answer::Refused(refused), by)
            })
        })
        .collect();
    for settled in &seen {
        eprintln!("client_settle_kill kill=refused {settled:?}");
    }
    for settled in seen {
        let start = settled.start;
        assert_eq!(settled.starts.len(), 1, "{start:?}");
        assert_eq!(
            settled.kills, settled.starts,
            "{start:?}: one Kill, of its exec"
        );
        assert_eq!(
            settled.result,
            Err(Refusal::Lifecycle {
                id: settled.id,
                cause: hex_api::LifecycleCause::Uncertain,
            }),
            "{start:?}: a refused kill leaves the program's end unproved"
        );
        assert_eq!(
            settled.releases, 0,
            "{start:?}: a program asked to end was released to run on"
        );
        assert!(settled.clean, "{start:?}");
    }
}

/// AC-T7.4.1: Do the same with the agent confirming each Kill; expect the settlement's own
/// refusal, and still no Release for a program asked to end.
#[test]
fn a_settle_failure_whose_kill_is_confirmed_keeps_its_refusal_and_never_releases() {
    let seen: Vec<Settled> = [SettleStart::RunBefore, SettleStart::Run, SettleStart::Exec]
        .into_iter()
        .map(|start| within(&SETTLED, |by| settle_failure(start, Answer::Done, by)))
        .collect();
    for settled in &seen {
        eprintln!("client_settle_kill kill=done {settled:?}");
    }
    for settled in seen {
        let start = settled.start;
        assert_eq!(settled.starts.len(), 1, "{start:?}");
        assert_eq!(
            settled.kills, settled.starts,
            "{start:?}: one Kill, of its exec"
        );
        assert!(
            matches!(&settled.result, Err(Refusal::Broken(why)) if why.contains("stdout") && why.contains("neither carried nor asked")),
            "{start:?}: a confirmed kill keeps the settlement's refusal: {:?}",
            settled.result
        );
        assert_eq!(
            settled.releases, 0,
            "{start:?}: a program asked to end was released to run on"
        );
        assert!(settled.clean, "{start:?}");
    }
}

/// AC-T7.4.1 control A: actual selection lends the setup seam the caller's deadline, never the
/// existing private smaller learned wait, whose end is a run-out to record, not the call's; that
/// deadline consumed at setup, no send is attempted and the refusal is the caller's expiry.
#[test]
fn selected_callers_setup_expiry_prevents_send() {
    /// The hello's return, its selected setup authority consumed before any send.
    static RETURNED: Patience =
        Patience::measuring("test.hex_client.budget.selected_setup_returned");
    let (transport, writes) = stalled(None);
    let client = Arc::new(configured(Calibrated::<false>(transport)));
    let control = Arc::clone(&client);
    let hello = Request::Hello {
        build: wire::build(),
    };
    let (selected, selection) = mpsc::sync_channel(1);
    let (finished, answer) = mpsc::sync_channel(1);
    let worker = std::thread::spawn(move || {
        let original = Instant::now().checked_add(CAP).unwrap();
        let cap = ReplyDeadline::caller_before(original).unwrap();
        let result = control
            .conn
            .call_before(
                wire::MAX_MESSAGE,
                &hello,
                |_, deadline| {
                    let deadline = deadline.ok_or_else(|| {
                        Refusal::Broken("selected setup deadline was absent".into())
                    })?;
                    selected
                        .try_send((original, deadline))
                        .map_err(|why| Refusal::os("observing selected setup authority", why))?;
                    // Deliberately consume exactly the selected authority, without
                    // replacing it or changing a seed. Caller::send must now emit nothing.
                    until(deadline.at())?;
                    Ok(NONE)
                },
                None,
                Some(cap),
            )
            .map(drop);
        let _ = finished.send((Instant::now(), result));
    });
    // Its selected deadline's notice precedes its answer, so it is read once the answer has come.
    let observed = recv_on(&RETURNED, &answer);
    let selected = selection.try_recv();
    client.conn.shared.end(Refusal::Broken(
        "owned selected-deadline observation cleanup".into(),
    ));
    let joined = worker.join();
    let writes = writes.try_recv();
    let clean =
        client.conn.shared.state().map(|state| {
            state.calls.is_empty() && state.programs.is_empty() && state.given.is_empty()
        });
    eprintln!(
        "client_hello_phase_a selected={selected:?} observed={observed:?} writes={writes:?} joined={} clean={clean:?}",
        joined.is_ok()
    );
    joined.unwrap();
    let (original, selected) = selected.unwrap();
    assert!(matches!(selected.source, ReplyBound::CallerAbsolute));
    assert_eq!(selected.at(), original);
    let (returned, result) = observed.unwrap();
    assert!(returned >= selected.at());
    assert!(matches!(
        result,
        Err(Refusal::Os { op: "sending to the node agent", why }) if why.contains("absolute") && !why.contains("twice")
    ));
    // This Stalled transport commits its whole nonempty input on its first try_send;
    // no notification therefore independently witnesses zero send calls/bytes.
    assert!(matches!(writes, Err(mpsc::TryRecvError::Empty)));
    assert!(clean.unwrap());
}

struct UnarmedOrdinaryHello {
    stream: guest::Stream,
    writes: Arc<Mutex<Vec<(usize, Instant)>>>,
}

impl Transport for UnarmedOrdinaryHello {
    type Reading = guest::Reading;
    const MAKES_ASKED: bool = <guest::Stream as Transport>::MAKES_ASKED;

    fn answers() -> &'static [Patience; KINDS] {
        static WAITS: [Patience; KINDS] =
            [const { Patience::measuring("test.ordinary_hello_phase.unarmed") }; KINDS];
        &WAITS
    }
    fn reply_budget(&self) -> Option<Duration> {
        self.stream.reply_budget()
    }
    fn reading(&self) -> Self::Reading {
        self.stream.reading()
    }
    fn handle(&self) -> Handle<'_> {
        self.stream.handle()
    }
    fn frame(message: Vec<u8>) -> Result<Vec<u8>, Refusal> {
        <guest::Stream as Transport>::frame(message)
    }
    fn recv(&self, reading: &mut Self::Reading, take: &mut Take<'_>) -> Result<bool, Refusal> {
        self.stream.recv(reading, take)
    }
    fn try_send(&self, bytes: &[u8], at: &mut usize, lent: &[Lent<'_>]) -> Result<bool, Sent> {
        let before = *at;
        let result = self.stream.try_send(bytes, at, lent);
        if *at > before {
            self.writes
                .lock()
                .unwrap()
                .push((*at - before, Instant::now()));
        }
        result
    }
    fn shutdown(&self) {
        self.stream.shutdown();
    }
}

/// One actual TCP frame, using the existing readiness helper and one original
/// deadline for every wait. No per-read timeout reset can extend this observation.
fn ordinary_hello_frame(
    peer: &mut TcpStream,
    deadline: ReplyDeadline,
) -> Result<(Header, Request, Vec<u8>), Refusal> {
    use hex_core::stream::Deframer;

    peer.set_nonblocking(true)
        .map_err(|why| Refusal::os("observing ordinary Hello without waiting", why))?;
    let mut decoder = Deframer::new(wire::MAX_MESSAGE);
    let mut buffer = vec![0; hex_core::stream::MAX_FRAME];
    let mut captured = Vec::new();
    loop {
        // Already available data is evidence even if observation is scheduled late;
        // only a new readiness wait consumes the remaining original authority.
        let n = match peer.read(&mut buffer) {
            Ok(0) => {
                return Err(Refusal::Broken(
                    "ordinary Hello peer ended mid-frame".into(),
                ));
            }
            Ok(n) => n,
            Err(why) if why.kind() == io::ErrorKind::Interrupted => {
                deadline.remaining("observing ordinary Hello")?;
                continue;
            }
            Err(why) if why.kind() == io::ErrorKind::WouldBlock => {
                protocol_ready(
                    Duplex::handle(peer),
                    false,
                    deadline,
                    "observing ordinary Hello",
                )?;
                continue;
            }
            Err(why) => return Err(Refusal::os("reading ordinary Hello", why)),
        };
        if captured.len() + n > hex_core::stream::MAX_FRAME {
            return Err(Refusal::Broken("ordinary Hello exceeded one frame".into()));
        }
        captured.extend_from_slice(&buffer[..n]);
        let mut input = &buffer[..n];
        while !input.is_empty() {
            let (took, message) = decoder.take(input).map_err(Refusal::from)?;
            if took == 0 || took > input.len() {
                return Err(Refusal::Broken(
                    "ordinary Hello parser made no progress".into(),
                ));
            }
            input = &input[took..];
            if let Some(message) = message {
                if !input.is_empty() || !decoder.at_boundary() {
                    return Err(Refusal::Broken("bytes after ordinary Hello frame".into()));
                }
                let (header, request) = wire::read_request_apart(&message, wire::MAX_MESSAGE)
                    .map_err(|bad| Refusal::from(bad.fault))?;
                return Ok((header, request, captured));
            }
        }
    }
}

/// AC-T7.4.1 control B: the SAME ordinary Stream/Core Hello path under a private
/// unarmed test census sends a full actual frame, gets no answer, and exhausts the
/// caller's original cap. This is NOT the public calibrated constructor fixture.
#[test]
fn isolated_ordinary_stream_hello_withheld_answer_uses_original_cap() {
    /// The hello's original deadline, sent as it begins.
    static BEGAN: Patience = Patience::measuring("test.hex_client.budget.ordinary_hello_began");
    /// The hello's refusal at its caller's cap, no answer come.
    static RETURNED: Patience =
        Patience::measuring("test.hex_client.budget.ordinary_hello_returned");
    let (socket, mut peer) = absolute_pair();
    peer.write_all(&11u64.to_le_bytes()).unwrap();
    let recorded = Arc::new(Mutex::new(Vec::new()));
    let link = guest::own_link(AbsoluteNamedLink {
        control: Mutex::new(Some(socket)),
        stdio: Mutex::new(VecDeque::new()),
        deadlines: Arc::clone(&recorded),
    })
    .unwrap();
    // The existing test-only dial consumes a complete prequeued token before the
    // isolated Hello operation. It makes no claim about the public factory's dial phase.
    let stream = guest::Stream::dial(link).unwrap();
    let writes = Arc::new(Mutex::new(Vec::new()));
    let shared = Shared::try_new(UnarmedOrdinaryHello {
        stream,
        writes: Arc::clone(&writes),
    })
    .unwrap();
    let control = triomphe::Arc::clone(&shared);
    let (starting, started) = mpsc::sync_channel(1);
    let (finished, answer) = mpsc::sync_channel(1);
    let worker = std::thread::spawn(move || {
        let original = Instant::now().checked_add(CAP).unwrap();
        let deadline = ReplyDeadline::caller_before(original).unwrap();
        let _ = starting.try_send(deadline);
        let result = Core::hello_before(control, Some(deadline)).map(drop);
        let _ = finished.send((Instant::now(), result));
    });
    let deadline = recv_on(&BEGAN, &started);
    // Not a wait for a fact: a frame is sent only within the hello's own deadline, so that deadline bounds its
    // read, and what has come already is read however late.
    let frame = match &deadline {
        Ok(deadline) => ordinary_hello_frame(&mut peer, *deadline),
        Err(why) => Err(Refusal::os("observing isolated Hello authority", why)),
    };
    // The peer has sent the token only: no Hello response is queued or synthesized.
    let observed = recv_on(&RETURNED, &answer);
    let stopped = peer.shutdown(Shutdown::Both);
    drop(peer);
    shared.end(Refusal::Broken("owned ordinary-Hello cleanup".into()));
    let joined = worker.join();
    let committed = writes.lock().unwrap();
    let clean = shared
        .state()
        .map(|state| state.calls.is_empty() && state.programs.is_empty() && state.given.is_empty());
    eprintln!(
        "client_hello_phase_b scope=isolated_ordinary_seam public_constructor_proof=false deadline={deadline:?} frame={frame:?} writes={committed:?} observed={observed:?} stopped={stopped:?} joined={} clean={clean:?}",
        joined.is_ok()
    );
    joined.unwrap();
    let deadline = deadline.unwrap();
    let (header, request, bytes) = frame.expect("full actual Hello is a required phase witness");
    assert_eq!((header.kind, header.id), (1, 1));
    assert!(matches!(request, Request::Hello { build } if build == wire::build()));
    assert_eq!(
        committed.iter().map(|(n, _)| *n).sum::<usize>(),
        bytes.len()
    );
    assert!(!committed.is_empty());
    let (returned, result) = observed.unwrap();
    assert!(returned >= deadline.at());
    assert!(matches!(
        result,
        Err(Refusal::Os { op: "waiting for the node agent's answer", why })
            if why.contains("absolute") && !why.contains("twice")
    ));
    assert_eq!(shared.transport.reply_budget(), None);
    assert!(clean.unwrap());
}

/// Separate successor transport: the same real memory protocol, with all phase diagnostics
/// deferred until cleanup. The original SettleOriginal and its selector above stay untouched.
struct SettleAligned {
    io: Offers,
    prepared: Mutex<Option<hex_api::Given>>,
    setup: Mutex<Option<(u64, hex_api::Ask, ReplyDeadline)>>,
    started: mpsc::SyncSender<(u64, Instant)>,
    kill: mpsc::SyncSender<(u64, u64, Instant)>,
    exits: AtomicUsize,
    other_answers: AtomicUsize,
}

impl Transport for SettleAligned {
    type Reading = ();
    const MAKES_ASKED: bool = true;

    fn answers() -> &'static [Patience; KINDS] {
        static WAITS: [Patience; KINDS] =
            [const { Patience::measuring("test.client_settle_aligned.unarmed") }; KINDS];
        &WAITS
    }

    fn reading(&self) {}
    fn handle(&self) -> Handle<'_> {
        self.io.handle()
    }

    fn recv(&self, reading: &mut (), take: &mut Take<'_>) -> Result<bool, Refusal> {
        let mut observed = |message: &Received<'_>, fds| {
            let (header, answer) =
                wire::read_answer(message, wire::MAX_MESSAGE).map_err(Refusal::from)?;
            take(message, fds)?;
            match answer {
                Answer::Started { .. } => {
                    let _ = self.started.try_send((header.id, Instant::now()));
                }
                Answer::Exited(_) => {
                    self.exits.fetch_add(1, Ordering::SeqCst);
                    self.other_answers.fetch_add(1, Ordering::SeqCst);
                }
                _ => {
                    self.other_answers.fetch_add(1, Ordering::SeqCst);
                }
            }
            Ok(())
        };
        let got = self.io.recv(reading, &mut observed)?;
        if self.io.io.closed.load(Ordering::Acquire) {
            Err(Refusal::Broken(
                "owned aligned settle transport closed".into(),
            ))
        } else {
            Ok(got)
        }
    }

    fn try_send(&self, bytes: &[u8], at: &mut usize, _: &[Lent<'_>]) -> Result<bool, Sent> {
        let (header, request) = wire::read_request_apart(bytes, wire::MAX_MESSAGE)
            .map_err(|bad| Sent::Refused(Refusal::from(bad.fault)))?;
        let killed = match &request {
            Request::Call(Call::Run { .. }) => {
                self.io.queue(header.id, Answer::Started { pid: 7 });
                None
            }
            // Record the actual committed Kill, but never manufacture its answer or an Exit.
            Request::Call(Call::Kill { exec }) => Some(*exec),
            Request::Release => None,
            _ => {
                return Err(Sent::Refused(Refusal::Broken(
                    "unexpected owned aligned-settle request".into(),
                )));
            }
        };
        self.io
            .outbound
            .lock()
            .map_err(|why| Sent::Refused(poisoned(why)))?
            .push((header.id, request));
        *at = bytes.len();
        if let Some(exec) = killed {
            let _ = self.kill.try_send((header.id, exec, Instant::now()));
        }
        Ok(true)
    }

    fn asks_before(
        &self,
        exec: u64,
        ask: hex_api::Ask,
        deadline: ReplyDeadline,
    ) -> Result<hex_api::Given, Refusal> {
        deadline.remaining("owned prepared malformed stream setup")?;
        *self.setup.lock().map_err(poisoned)? = Some((exec, ask, deadline));
        self.prepared
            .lock()
            .map_err(poisoned)?
            .take()
            .ok_or_else(|| Refusal::Broken("prepared wrong end was already taken".into()))
    }

    fn carries_before(&self, _: u64, stdio: Stdio, deadline: ReplyDeadline) -> Result<(), Refusal> {
        deadline.remaining("owned null carried streams")?;
        stdio.check()?;
        if carried(&stdio) != Streams::default() {
            return Err(Refusal::Broken("unexpected owned carried pipe".into()));
        }
        Ok(())
    }

    fn shutdown(&self) {
        self.io.shutdown();
    }
}

/// AC-T7.4.1 separately pinned successor: start the same cap in an already prepared actor.
/// ROOT must compare this SAME selector before/after69a and under the missing-Kill-cap mutation.
#[test]
fn prepared_actor_settle_error_retains_original_kill_answer_authority() {
    /// The prepared actor's notice that it is ready.
    static READY: Patience = Patience::measuring("test.hex_client.budget.prepared_ready");
    /// The prepared run's return, its settle refused and its Kill unanswered within its original authority.
    static RETURNED: Patience = Patience::measuring("test.hex_client.budget.prepared_returned");
    let (io, _) = stalled(None);
    let (starting, started) = mpsc::sync_channel(1);
    let (killing, killed) = mpsc::sync_channel(1);
    let (prepared, readiness) = mpsc::sync_channel(1);
    let (release, begin) = mpsc::sync_channel(1);
    let (arming, armed) = mpsc::sync_channel(1);
    let (finished, answer) = mpsc::sync_channel(1);
    let observer = Wake::new().unwrap();
    let mut inbound = VecDeque::new();
    inbound.try_reserve(1).unwrap();
    let mut outbound = Vec::new();
    outbound.try_reserve_exact(2).unwrap();
    let mut program_ends = Stdio {
        ask: hex_api::Ask {
            stdout: true,
            ..hex_api::Ask::default()
        },
        ..Stdio::null()
    };
    let wrong_end = program_ends.make_asked().unwrap();
    let client = Arc::new(configured(SettleAligned {
        io: Offers {
            inbound: Mutex::new(inbound),
            outbound: Mutex::new(outbound),
            program: Mutex::new(Some(program_ends)),
            ..Offers::unobserved(io)
        },
        prepared: Mutex::new(Some(wrong_end)),
        setup: Mutex::new(None),
        started: starting,
        kill: killing,
        exits: AtomicUsize::new(0),
        other_answers: AtomicUsize::new(0),
    }));
    let control = Arc::clone(&client);
    let spec = absolute_spec();
    let program = Program::of(&["owned-prepared-settle"]);
    let stdio = Stdio {
        ask: hex_api::Ask {
            stdin: true,
            ..hex_api::Ask::default()
        },
        ..Stdio::null()
    };
    let worker = std::thread::Builder::new()
        .name("owned-prepared-settle".into())
        .spawn(move || {
            if prepared.try_send(()).is_err() || begin.recv().is_err() {
                return;
            }
            let Some(original) = Instant::now().checked_add(CAP) else {
                return;
            };
            // Nonblocking notices use preallocated channels; no diagnostics or argument
            // construction separates this one anchor from the actual Core operation.
            if arming.try_send(original).is_err() {
                return;
            }
            let result = control.run_before(spec, program, stdio, original).map(drop);
            let returned = Instant::now();
            let _ = finished.try_send((returned, result));
        })
        .unwrap();

    let actor_ready = recv_on(&READY, &readiness);
    let released = actor_ready.is_ok().then(|| release.try_send(()));
    // Every early branch closes this owned barrier so joining can never wait for a lost start.
    drop(release);
    // Its arming, its Started and its Kill precede its answer, so each is read once the answer has come.
    let before_cleanup = recv_on(&RETURNED, &answer);
    let original = armed.try_recv();
    let actual_started = started.try_recv();
    let actual_kill = killed.try_recv();
    let expired = original.as_ref().ok().map(|at| {
        loop {
            let left = at.saturating_duration_since(Instant::now());
            if left.is_zero() {
                break Ok(());
            }
            // Same existing readiness wait as until(), with its wake prepared before the cap.
            if let Err(why) = ready(observer.handle(), false, None, Some(left)) {
                break Err(why);
            }
        }
    });
    let cleanup_at = Instant::now();
    client
        .conn
        .shared
        .end(Refusal::Broken("owned prepared-settle cleanup".into()));
    let transport = &client.conn.shared.transport;
    let (pipe, pipe_poisoned) = match transport.io.program.lock() {
        Ok(mut program) => (program.take(), false),
        Err(poisoned) => (poisoned.into_inner().take(), true),
    };
    let (unused, end_poisoned) = match transport.prepared.lock() {
        Ok(mut given) => (given.take(), false),
        Err(poisoned) => (poisoned.into_inner().take(), true),
    };
    let pipe_was_owned = pipe.is_some();
    let unused_end = unused.is_some();
    drop((pipe, unused));
    let joined = worker.join();
    let after_cleanup = answer.try_recv();
    let setup = *transport.setup.lock().unwrap();
    let exits = transport.exits.load(Ordering::SeqCst);
    let other_answers = transport.other_answers.load(Ordering::SeqCst);
    let sent = transport.io.outbound.lock().unwrap();
    let runs: Vec<_> = sent
        .iter()
        .filter(|(_, request)| matches!(request, Request::Call(Call::Run { .. })))
        .map(|(id, _)| *id)
        .collect();
    let kills: Vec<_> = sent
        .iter()
        .filter_map(|(id, request)| match request {
            Request::Call(Call::Kill { exec }) => Some((*id, *exec)),
            _ => None,
        })
        .collect();
    let releases = sent
        .iter()
        .filter(|(_, request)| matches!(request, Request::Release))
        .count();
    let clean = client.conn.shared.state().map(|state| {
        state.calls.is_empty()
            && state.programs.is_empty()
            && state.given.is_empty()
            && state.waiting.is_empty()
    });
    let phase = actor_ready.is_ok()
        && released.as_ref().is_some_and(|sent| sent.is_ok())
        && original.as_ref().is_ok_and(|at| {
            runs.len() == 1
                && setup.is_some_and(|(exec, ask, cap)| {
                    exec == runs[0] && ask.stdin && !ask.stdout && !ask.stderr && cap.at() == *at
                })
                && actual_started
                    .as_ref()
                    .is_ok_and(|(exec, seen)| *exec == runs[0] && seen < at)
                && actual_kill.as_ref().is_ok_and(|(request, exec, seen)| {
                    *exec == runs[0] && seen < at && kills == [(*request, *exec)]
                })
        })
        && pipe_was_owned
        && !unused_end;
    // First diagnostics and all assertions occur only after the owned endpoint/ends close and join.
    eprintln!(
        "client_settle_prepared classification={} scope=separate_successor ready={actor_ready:?} released={released:?} original={original:?} cap={CAP:?} setup={setup:?} started={actual_started:?} kill={actual_kill:?} runs={runs:?} kills={kills:?} exits={exits} other_answers={other_answers} releases={releases} before_cleanup={before_cleanup:?} cleanup_at={cleanup_at:?} after_cleanup={after_cleanup:?} pipe_owned={pipe_was_owned} unused_end={unused_end} pipe_poisoned={pipe_poisoned} end_poisoned={end_poisoned} joined={} clean={clean:?}",
        if phase {
            "PHASE_WITNESSED"
        } else {
            "UNVERIFIED"
        },
        joined.is_ok()
    );
    joined.unwrap();
    assert!(!pipe_poisoned && !end_poisoned);
    assert!(transport.io.io.closed.load(Ordering::Acquire));
    assert!(transport.io.program.lock().unwrap().is_none());
    assert!(transport.prepared.lock().unwrap().is_none());
    assert!(clean.unwrap());
    assert_eq!(transport.reply_budget(), None);
    assert!(
        phase,
        "UNVERIFIED: required actual prepared-settle phase missing"
    );
    assert_eq!(exits, 0);
    assert_eq!(other_answers, 0);
    assert_eq!(releases, 0);
    expired.unwrap().unwrap();
    let returned_error = before_cleanup
        .as_ref()
        .ok()
        .or_else(|| after_cleanup.as_ref().ok());
    assert!(
        matches!(returned_error, Some((_, Err(Refusal::Lifecycle { id, cause: hex_api::LifecycleCause::Uncertain }))) if *id == absolute_spec().id),
        "a Kill unanswered within the original authority leaves the run's end uncertain"
    );
    assert!(
        before_cleanup.is_ok(),
        "prepared-settle original error return exceeded its original authority awaiting Kill answer"
    );
    let (returned, _) = before_cleanup.unwrap();
    assert!(returned >= original.unwrap());
    assert!(returned <= cleanup_at);
}

/// A call that gave up waiting: its late answers have no taker.
const GAVE_UP: u64 = u64::MAX;
/// Messages for others before the fuse ends the connection, the only way a poll that keeps reading
/// comes back: far more than these tests' polls and calls read (one each).
const FUSE: usize = 1_000;

/// Answers for others that keep coming: each read hands on one message, as one `SOCK_SEQPACKET` read
/// does, and leaves another ready.
struct Chatter {
    /// Its readiness: a wake written is readable once written, on macOS too, where a loopback
    /// datagram can arrive after its send returns and a zero-wait poll would miss it.
    socket: Wake,
    /// The agent's next messages, ahead of the traffic: answers to the test's calls, a program's end.
    next: Mutex<VecDeque<Vec<u8>>>,
    /// Whether the traffic is on: one late answer to [`GAVE_UP`] per read.
    chatting: AtomicBool,
    /// Turns taken: each waits on the connection through its handle once.
    turns: AtomicUsize,
    /// Messages for others handed on.
    others: AtomicUsize,
    sent: Mutex<Vec<(u64, Request)>>,
}

impl Chatter {
    fn new() -> Self {
        Self {
            socket: Wake::new().unwrap(),
            next: Mutex::new(VecDeque::new()),
            chatting: AtomicBool::new(false),
            turns: AtomicUsize::new(0),
            others: AtomicUsize::new(0),
            sent: Mutex::new(Vec::new()),
        }
    }

    /// `answer` to `id` is the agent's next message, ahead of the traffic.
    fn queue(&self, id: u64, answer: &Answer) {
        self.next
            .lock()
            .unwrap()
            .push_back(wire::answer(id, answer, wire::MAX_MESSAGE).unwrap());
        self.socket.wake();
    }

    fn chat(&self) {
        self.chatting.store(true, Ordering::SeqCst);
        self.socket.wake();
    }

    /// What `poll` returned, with the turns it took and the messages for others it read.
    fn counted<R>(&self, poll: impl FnOnce() -> R) -> (R, usize, usize) {
        let (turns, others) = (
            self.turns.load(Ordering::SeqCst),
            self.others.load(Ordering::SeqCst),
        );
        let got = poll();
        (
            got,
            self.turns.load(Ordering::SeqCst) - turns,
            self.others.load(Ordering::SeqCst) - others,
        )
    }
}

impl Transport for Chatter {
    type Reading = ();
    fn answers() -> &'static [Patience; KINDS] {
        static WAITS: [Patience; KINDS] =
            [const { Patience::measuring("test.client_poll.unarmed") }; KINDS];
        &WAITS
    }
    fn reading(&self) {}
    fn handle(&self) -> Handle<'_> {
        self.turns.fetch_add(1, Ordering::SeqCst);
        self.socket.handle()
    }
    fn recv(&self, _: &mut (), take: &mut Take<'_>) -> Result<bool, Refusal> {
        self.socket.drain();
        let next = self.next.lock().unwrap().pop_front();
        let message = match next {
            Some(message) => message,
            None if self.chatting.load(Ordering::SeqCst) => {
                if self.others.load(Ordering::SeqCst) >= FUSE {
                    return Err(Refusal::Broken(
                        "the fuse: a poll kept reading answers for others".into(),
                    ));
                }
                self.others.fetch_add(1, Ordering::SeqCst);
                wire::answer(GAVE_UP, &Answer::Done, wire::MAX_MESSAGE).unwrap()
            }
            None => return Ok(false),
        };
        take(
            &Received {
                size: message.len(),
                bytes: &message,
                descriptors: 0,
                descriptors_cut: false,
            },
            Vec::new(),
        )?;
        if self.chatting.load(Ordering::SeqCst) || !self.next.lock().unwrap().is_empty() {
            self.socket.wake();
        }
        Ok(true)
    }
    fn try_send(&self, bytes: &[u8], at: &mut usize, _: &[Lent<'_>]) -> Result<bool, Sent> {
        let (header, request) = wire::read_request_apart(bytes, wire::MAX_MESSAGE).unwrap();
        match &request {
            Request::Call(Call::Exec { .. } | Call::Run { .. }) => {
                self.queue(header.id, &Answer::Started { pid: 7 })
            }
            Request::Call(Call::Kill { .. }) => self.queue(header.id, &Answer::Done),
            _ => {}
        }
        self.sent.lock().unwrap().push((header.id, request));
        *at = bytes.len();
        Ok(true)
    }
    fn shutdown(&self) {}
}

/// A program started through the real exec call, and its exec's id.
fn chatting_program(client: &Core<Chatter>) -> (Box<dyn Proc>, u64) {
    let proc = client
        .exec(
            &BoxId::new("poll-bound").unwrap(),
            Program::of(&["poll-bound"]),
            Stdio::null(),
        )
        .unwrap();
    let sent = client.conn.shared.transport.sent.lock().unwrap();
    assert!(matches!(sent[..], [(_, Request::Call(Call::Exec { .. }))]));
    (proc, sent[0].0)
}

/// AC-T7.4.1: do `try_wait`, then `wait_for(0)`, on a remote program while answers for others keep
/// coming; expect each to take one turn, read one message and return `Ok(None)` with more still
/// coming. Then with its end next: expect the end from one turn, then from none.
#[test]
fn a_poll_reads_once_while_answers_for_others_keep_coming() {
    let client = configured(Chatter::new());
    let (mut proc, exec) = chatting_program(&client);
    let chatter = &client.conn.shared.transport;
    chatter.chat();
    let polled = chatter.counted(|| proc.try_wait());
    let zero = chatter.counted(|| proc.wait_for(Duration::ZERO));
    chatter.queue(exec, &Answer::Exited(Exit::Code(7)));
    let ended = chatter.counted(|| proc.try_wait());
    let again = chatter.counted(|| proc.try_wait());
    let others = chatter.others.load(Ordering::SeqCst);
    let state = client.conn.shared.state().unwrap();
    let handed_on = !state.reading && state.waiting.is_empty() && state.closed.is_none();
    drop(state);
    eprintln!(
        "client_poll_bound phase=try_wait polled={polled:?} zero={zero:?} ended={ended:?} again={again:?} others={others} fuse={FUSE} handed_on={handed_on}"
    );
    assert_eq!(
        polled,
        (Ok(None), 1, 1),
        "a poll must take one turn and read once while answers for others keep coming"
    );
    assert_eq!(zero, (Ok(None), 1, 1));
    assert_eq!(ended, (Ok(Some(Exit::Code(7))), 1, 0));
    assert_eq!(again, (Ok(Some(Exit::Code(7))), 0, 0));
    assert!(others < FUSE, "answers for others were still coming");
    assert!(handed_on, "each poll handed the reader's turn on");
}

/// AC-T7.4.1: do `kill` on a remote program while answers for others keep coming; expect its check
/// for an end to take one turn and read once, then exactly one Kill, answered in one more turn.
#[test]
fn kill_checks_once_for_an_end_while_answers_for_others_keep_coming() {
    let client = configured(Chatter::new());
    let (mut proc, exec) = chatting_program(&client);
    let chatter = &client.conn.shared.transport;
    chatter.chat();
    let killed = chatter.counted(|| proc.kill());
    let kills: Vec<u64> = chatter
        .sent
        .lock()
        .unwrap()
        .iter()
        .filter_map(|(_, request)| match request {
            Request::Call(Call::Kill { exec }) => Some(*exec),
            _ => None,
        })
        .collect();
    let others = chatter.others.load(Ordering::SeqCst);
    eprintln!(
        "client_poll_bound phase=kill killed={killed:?} kills={kills:?} exec={exec} others={others} fuse={FUSE}"
    );
    assert_eq!(
        killed,
        (Ok(()), 2, 1),
        "kill's check for an end must take one turn and read once, then one turn for its answer"
    );
    assert_eq!(kills, [exec]);
    assert!(others < FUSE, "answers for others were still coming");
}

/// AC-T7.4.1: do `wait_for` a microsecond on a remote program while answers for others keep coming;
/// expect `Ok(None)` once its time is spent, each turn reading once, never reading on until the fuse
/// ends the connection. Then with its end next: expect a spent wait to take that end from its read.
#[test]
fn a_timed_wait_ends_at_its_deadline_while_answers_for_others_keep_coming() {
    let client = configured(Chatter::new());
    let (mut proc, exec) = chatting_program(&client);
    let chatter = &client.conn.shared.transport;
    chatter.chat();
    let wait = Duration::from_micros(1);
    let waited = chatter.counted(|| proc.wait_for(wait));
    chatter.queue(exec, &Answer::Exited(Exit::Code(3)));
    let ended = proc.wait_for(wait);
    eprintln!("client_wait_bound phase=wait_for waited={waited:?} ended={ended:?} fuse={FUSE}");
    assert_eq!(
        waited.0,
        Ok(None),
        "a timed wait came back with its time spent"
    );
    assert!(
        waited.2 < FUSE,
        "it read on until the fuse ended the connection"
    );
    assert_eq!(waited.1, waited.2, "each turn read once");
    assert_eq!(ended, Ok(Some(Exit::Code(3))));
}

/// A Chatter whose transport makes an exec's asked streams, and gives the wrong one: asked for
/// stdin, it gives stdout's end, so a run's setup fails after its start and its kill must go out.
struct WrongEnd(Chatter);

impl Transport for WrongEnd {
    type Reading = ();
    const MAKES_ASKED: bool = true;
    fn answers() -> &'static [Patience; KINDS] {
        Chatter::answers()
    }
    fn reading(&self) {}
    fn handle(&self) -> Handle<'_> {
        self.0.handle()
    }
    fn recv(&self, reading: &mut (), take: &mut Take<'_>) -> Result<bool, Refusal> {
        self.0.recv(reading, take)
    }
    fn try_send(&self, bytes: &[u8], at: &mut usize, lent: &[Lent<'_>]) -> Result<bool, Sent> {
        self.0.try_send(bytes, at, lent)
    }
    fn shutdown(&self) {}
    fn carries_before(&self, _: u64, stdio: Stdio, deadline: ReplyDeadline) -> Result<(), Refusal> {
        deadline.remaining("carrying no streams")?;
        assert_eq!(
            carried(&stdio),
            Streams::default(),
            "this run carries no pipe"
        );
        Ok(())
    }
    fn asks_before(
        &self,
        _: u64,
        _: hex_api::Ask,
        deadline: ReplyDeadline,
    ) -> Result<hex_api::Given, Refusal> {
        deadline.remaining("making the wrong stream")?;
        Stdio {
            ask: hex_api::Ask {
                stdout: true,
                ..hex_api::Ask::default()
            },
            ..Stdio::null()
        }
        .make_asked()
    }
}

/// AC-T7.4.1: do `run_before`, with a deadline the test never reaches, a program whose stream setup
/// fails after its start (stdout's end given where stdin was asked for) while answers for others keep
/// coming; expect the setup's refusal, its kill's check for an end to take one turn and read once,
/// and exactly one Kill for its run, answered in one more turn.
#[test]
fn a_failed_setups_kill_goes_out_while_answers_for_others_keep_coming() {
    let client = configured(WrongEnd(Chatter::new()));
    let chatter = &client.conn.shared.transport.0;
    chatter.chat();
    let far = Instant::now()
        .checked_add(Duration::from_secs(3600))
        .unwrap();
    let ran = chatter.counted(|| {
        client
            .run_before(
                absolute_spec(),
                Program::of(&["wrong-end"]),
                Stdio {
                    ask: hex_api::Ask {
                        stdin: true,
                        ..hex_api::Ask::default()
                    },
                    ..Stdio::null()
                },
                far,
            )
            .map(drop)
    });
    let sent = chatter.sent.lock().unwrap();
    let run = sent
        .iter()
        .find(|(_, request)| matches!(request, Request::Call(Call::Run { .. })))
        .map(|(id, _)| *id);
    let kills: Vec<u64> = sent
        .iter()
        .filter_map(|(_, request)| match request {
            Request::Call(Call::Kill { exec }) => Some(*exec),
            _ => None,
        })
        .collect();
    eprintln!(
        "client_wait_bound phase=setup_kill ran={ran:?} run={run:?} kills={kills:?} fuse={FUSE}"
    );
    assert!(
        matches!(&ran.0, Err(Refusal::Broken(why)) if why.contains("stdout")),
        "the setup's own refusal, its kill confirmed: {:?}",
        ran.0
    );
    assert_eq!(
        (ran.1, ran.2),
        (3, 1),
        "the run's answer, the kill's one check for an end, the kill's answer"
    );
    assert_eq!(kills, run.into_iter().collect::<Vec<_>>());
}

/// Whether the pipe `program` writes has no reader left, waited for as a fact on its own patience: another
/// test's child holds a copy of every descriptor from its fork to its exec, which closes it (CLOEXEC), so a
/// write in between would still find a reader. The pipe is filled first and nothing reads it, so a poll for
/// room returns only once its last reader has gone: `POLLERR` on Linux (fs/pipe.c's `pipe_poll`), `POLLHUP` on
/// macOS, whose poll reports nothing for an end asking for no event.
#[cfg(unix)]
fn readerless(program: &std::io::PipeWriter) -> bool {
    use rustix::event::{PollFd, PollFlags};
    use rustix::io::Errno;
    /// The late end's pipe with its last reader gone.
    static READERLESS: Patience = Patience::measuring("test.hex_client.budget.late_end_readerless");
    rustix::io::ioctl_fionbio(program, true).unwrap();
    loop {
        match rustix::io::write(program, &[0; 4096]) {
            Ok(_) => {}
            Err(Errno::AGAIN) => break,
            Err(Errno::PIPE) => return true,
            Err(e) => panic!("filling the late end's pipe: {e}"),
        }
    }
    let mut room = [PollFd::new(program, PollFlags::OUT)];
    let polled = within(&READERLESS, |by| {
        let left = by.map(|by| timespec(by.saturating_duration_since(Instant::now())));
        rustix::event::poll(&mut room, left.as_ref())
    });
    polled.unwrap() == 1
        && room[0]
            .revents()
            .intersects(PollFlags::ERR | PollFlags::HUP)
}

/// Windows lends a child only the handles named for it, so a write finds the pipe's last reader gone at once.
#[cfg(windows)]
fn readerless(program: &std::io::PipeWriter) -> bool {
    matches!((&*program).write(&[1]), Err(e) if e.kind() == io::ErrorKind::BrokenPipe)
}

/// An agent whose answers come only as the test queues them, each with the ends it gives (a given
/// answer's descriptors), but for a list's, which comes at once.
struct Late {
    wake: Wake,
    next: Mutex<VecDeque<(Vec<u8>, Vec<hex_api::End>)>>,
    sent: Mutex<Vec<(u64, Request)>>,
}

impl Late {
    fn queue(&self, id: u64, answer: &Answer, ends: Vec<hex_api::End>) {
        let bytes = wire::answer(id, answer, wire::MAX_MESSAGE).unwrap();
        self.next.lock().unwrap().push_back((bytes, ends));
        self.wake.wake();
    }
}

impl Transport for Late {
    type Reading = ();
    fn answers() -> &'static [Patience; KINDS] {
        static WAITS: [Patience; KINDS] =
            [const { Patience::measuring("test.client_late_given.unarmed") }; KINDS];
        &WAITS
    }
    fn reading(&self) {}
    fn handle(&self) -> Handle<'_> {
        self.wake.handle()
    }
    fn recv(&self, _: &mut (), take: &mut Take<'_>) -> Result<bool, Refusal> {
        self.wake.drain();
        let mut came = false;
        loop {
            let Some((bytes, ends)) = self.next.lock().unwrap().pop_front() else {
                return Ok(came);
            };
            let message = Received {
                size: bytes.len(),
                bytes: &bytes,
                descriptors: ends.len(),
                descriptors_cut: false,
            };
            take(&message, ends)?;
            came = true;
        }
    }
    fn try_send(&self, bytes: &[u8], at: &mut usize, _: &[Lent<'_>]) -> Result<bool, Sent> {
        let (header, request) = wire::read_request_apart(bytes, wire::MAX_MESSAGE).unwrap();
        if request == Request::Call(Call::List) {
            let none = Answer::Boxes {
                infos: Vec::new(),
                more: false,
            };
            self.queue(header.id, &none, Vec::new());
        }
        self.sent.lock().unwrap().push((header.id, request));
        *at = bytes.len();
        Ok(true)
    }
    fn shutdown(&self) {}
}

/// AC-T7.4.1: do give up on a run asking for its stdout at its deadline, the agent answering only
/// after, with stdout's end, then make a list that reads that late answer before its own; expect
/// the run's caller expiry, the list answered, nothing kept for the run, the late end closed (its
/// pipe has no reader left), and the Taken word for it sent, so the agent lets its copy go.
#[test]
fn a_given_answer_after_its_caller_gave_up_closes_its_ends() {
    let client = configured(Late {
        wake: Wake::new().unwrap(),
        next: Mutex::new(VecDeque::new()),
        sent: Mutex::new(Vec::new()),
    });
    let late = &client.conn.shared.transport;
    let gave_up = client
        .run_before(
            absolute_spec(),
            Program::of(&["late-given"]),
            Stdio {
                ask: hex_api::Ask {
                    stdout: true,
                    ..hex_api::Ask::default()
                },
                ..Stdio::null()
            },
            Instant::now().checked_add(CAP).unwrap(),
        )
        .map(drop);
    let run = late.sent.lock().unwrap()[0].0;
    let (output, program) = std::io::pipe().unwrap();
    let given = Answer::Given {
        pid: 9,
        given: Streams {
            stdout: true,
            ..Streams::default()
        },
    };
    late.queue(run, &given, vec![output.into()]);
    let listed = client.list();
    let kept = client.conn.shared.state().unwrap().given.contains_key(&run);
    // Kept, it would never close: only a dropped end can be waited out.
    let closed = !kept && readerless(&program);
    let taken = late
        .sent
        .lock()
        .unwrap()
        .iter()
        .any(|(id, request)| *id == run && *request == Request::Taken);
    eprintln!(
        "client_late_given gave_up={gave_up:?} listed={listed:?} kept={kept} closed={closed} taken={taken}"
    );
    assert!(
        matches!(&gave_up, Err(Refusal::Os { why, .. }) if why.contains("absolute")),
        "{gave_up:?}"
    );
    assert_eq!(listed, Ok(Vec::new()));
    assert!(
        !kept,
        "the late answer's ends were kept for a run no one waits on"
    );
    assert!(closed, "the late stdout end is still open");
    assert!(taken, "no Taken word for the late answer");
}
