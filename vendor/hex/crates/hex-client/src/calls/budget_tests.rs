//! AC-T7.4.1: caller caps at the actual Core/guest setup seam, no VMM or GPU.
//!
//! Each wait here for something that should happen takes its own fact's patience (hex_testkit::waits; the lead's
//! rules of 2026-11-06 and 2026-11-08), never a picked window: no recorded operation makes any of these facts, so each
//! measures, for its fact alone, named `test.hex_client.budget.<fact>`, its answers recorded for the census that will
//! seed it. A capped call ends on its own cap however late its answer reaches the test. A window stays only where it is
//! the claim under test, and says so.

use super::*;
use crate::guest::{self, Duplex, DuplexOwner, Link, own_duplex};
use hex_testkit::waits::follow::{self, Party};
use std::io::{self, Read, Write};
use std::net::{Ipv4Addr, Shutdown, SocketAddr, TcpListener, TcpStream, UdpSocket};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::mpsc;

/// What a learned wait's refusal says, which a caller's own refusal never does: the slowest answer to a call of its
/// kind this process has seen is its bound (decision 9).
const LEARNED: &str = "the slowest answer to a call of its kind";

/// The next message from `from` by `by`, or with no deadline whenever it comes; a sender gone is `Disconnected`.
fn recv_by<T>(from: &mpsc::Receiver<T>, by: Option<Instant>) -> Result<T, mpsc::RecvTimeoutError> {
    match by {
        Some(by) => from.recv_timeout(by.saturating_duration_since(Instant::now())),
        None => from
            .recv()
            .map_err(|mpsc::RecvError| mpsc::RecvTimeoutError::Disconnected),
    }
}

/// Waits on `wait` for `what`, the next message from `from`, by the life of its senders (the review's C-5): the
/// channel's end, once every sender has dropped with its thread, is the party's, which the receive sees itself.
fn recv_on<T>(wait: &Patience, what: &str, from: &mpsc::Receiver<T>) -> Result<T, follow::Unmet> {
    let ended = std::cell::Cell::new(false);
    follow::within(
        wait,
        what,
        || {
            if ended.get() {
                Party::Ended
            } else {
                Party::Live
            }
        },
        |by| match recv_by(from, by) {
            Ok(got) => Some(got),
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                ended.set(true);
                None
            }
            Err(_) => None,
        },
    )
}

/// An io call's answer, as a slice of [`follow::within`] takes it: its own deadline passing is no answer yet.
fn io_slice<T>(got: io::Result<T>) -> Option<io::Result<T>> {
    match got {
        Err(e) if e.kind() == io::ErrorKind::TimedOut => None,
        got => Some(got),
    }
}

/// Waits on `wait` for the next message from `from`, following `sender`, the thread that sends it, by its life
/// (decision 9): past W* the wait goes on while the thread lives, and fails only once it has ended with nothing sent.
fn recv_from<T>(
    wait: &Patience,
    what: &str,
    from: &mpsc::Receiver<T>,
    sender: &std::thread::JoinHandle<()>,
) -> Result<T, follow::Unmet> {
    follow::within(
        wait,
        what,
        || {
            if sender.is_finished() {
                Party::Ended
            } else {
                Party::Live
            }
        },
        |by| recv_by(from, by).ok(),
    )
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

/// A capped operation's cap as this host measures it, decision 9's W*, and the operation's answer (Ada's rule of
/// 2026-11-13: no budget of a test's is fixed). `operation` is the capped one carried to its answer under a cap no answer
/// here comes near ([`MOST_ONE_WAIT`], so it takes the capped path, every deadline step of it), run by a worker thread
/// of its own; its time from that thread's spawn to its answer heard here is a sample of `steps`, and the cap is the
/// slowest `steps` has seen, the first sample its cold start. The cap's value is coverage alone (ROOT, 2026-11-14): the
/// verdict rests on what the client armed, every wait at one instant, and on its refusal never coming before that
/// instant; a host that holds the capped thread off a core past its cap gives the cut path, told to the census.
fn measured<T: Send + 'static>(
    steps: &'static Patience,
    operation: impl FnOnce() -> T + Send + 'static,
) -> (Duration, T) {
    /// The measured operation's answer, heard from its worker.
    static ANSWERED: Patience = Patience::measuring("test.hex_client.budget.measured_answered");
    let began = Instant::now();
    let (sent, got) = mpsc::sync_channel(1);
    let worker = std::thread::spawn(move || {
        let _ = sent.send(operation());
    });
    let answer = recv_from(&ANSWERED, "the measured operation's answer", &got, &worker);
    let took = began.elapsed();
    worker.join().unwrap();
    hex_testkit::waits::answered(steps, took);
    let cap = steps.deadline().unwrap_or(took);
    eprintln!("client_budget_cap took={took:?} w_star={cap:?}");
    (cap, answer.unwrap())
}

/// The census's line of the path the host gave a capped operation (ROOT, 2026-11-14: coverage, held to no bound):
/// `progressed`, the steps its claim needs made before its cap, or `cut`, its cap passing first.
fn told_path(test: &str, progressed: bool) {
    eprintln!(
        "client_budget_path test={test} path={}",
        if progressed { "progressed" } else { "cut" }
    );
}

/// The instant an operation's armed waits took, read from its log's first entry that isn't a learned wait's: none where
/// it armed none.
fn armed_instant(armed: &[Armed]) -> Option<Instant> {
    armed
        .iter()
        .find(|armed| !armed.learned)
        .and_then(|armed| armed.at)
}

/// Whether an operation's armed log reached `what`, a wait or step its claim needs.
fn reached(armed: &[Armed], what: &str) -> bool {
    armed.iter().any(|armed| armed.what == what)
}

/// A [`BootstrapProbe`] whose learned waits are its own, its plain kinds' and every class's box kinds'. The measured
/// calls' ([`measured`], `SEAM` false): what they teach no wait a capped call has, so an unarmed one stays unarmed. The
/// armed log's own test's (`SEAM` true): no other test's call touches them, so none's run-out, which has the next wait
/// follow its party with no learned bound until an answer comes (decision 9), takes away the learned wait it reads.
struct Apart<const SEAM: bool>(BootstrapProbe);
impl<const SEAM: bool> Transport for Apart<SEAM> {
    type Reading = ();
    fn answers() -> &'static [Patience; KINDS] {
        static MEASURED: [Patience; KINDS] =
            [const { Patience::measuring("test.client_measured") }; KINDS];
        static SEAM_WAITS: [Patience; KINDS] =
            [const { Patience::measuring("test.client_seam") }; KINDS];
        if SEAM { &SEAM_WAITS } else { &MEASURED }
    }
    fn class_answers(class: Class) -> &'static [Patience; BOX_KINDS] {
        static MEASURED: [[Patience; BOX_KINDS]; CLASSES] = [
            class_waits!("test.client_measured", "process", measuring),
            class_waits!("test.client_measured", "oci", measuring),
            class_waits!("test.client_measured", "firecracker", measuring),
            class_waits!("test.client_measured", "shards", measuring),
        ];
        static SEAM_WAITS: [[Patience; BOX_KINDS]; CLASSES] = [
            class_waits!("test.client_seam", "process", measuring),
            class_waits!("test.client_seam", "oci", measuring),
            class_waits!("test.client_seam", "firecracker", measuring),
            class_waits!("test.client_seam", "shards", measuring),
        ];
        by_class(if SEAM { &SEAM_WAITS } else { &MEASURED }, class)
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

/// A probe that answers one call with `reply` as it is asked, and gives each call `budget`, its learned waits apart
/// ([`Apart`]): a measured call's transport.
fn answering(reply: Answer, budget: Option<Duration>) -> Apart<false> {
    let (io, _) = stalled(None);
    Apart(BootstrapProbe {
        io: Offers::unobserved(io),
        reply: Mutex::new(Some(reply)),
        budget,
    })
}

/// No boxes: a list's answer.
fn no_boxes() -> Answer {
    Answer::Boxes {
        infos: Vec::new(),
        more: false,
    }
}

/// The relay's token on every guest connection here.
const TOKEN: u64 = 11;

/// A guest's control connection made before its dial: the relay's token and `answers` (each a call's id and its
/// answer) already written by its peer, which the test keeps.
fn control_pair(answers: Vec<(u64, Answer)>) -> (TcpStream, TcpStream) {
    let (socket, mut peer) = absolute_pair();
    peer.write_all(&TOKEN.to_le_bytes()).unwrap();
    for (id, answer) in answers {
        answer_to(&mut peer, id, answer).unwrap();
    }
    (socket, peer)
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

/// Do send an actual Core hello with an explicit caller cap and an unarmed learned wait; expect caller expiry instead of
/// waiting until the external original-control release: every wait the hello armed, in its own thread, at its one
/// deadline (its ask and the cap), and its refusal the caller's, never before that deadline. The cap is this host's own
/// ([`measured`]: the same hello, answered); whether the host let the hello's send come before it is told to the census.
#[test]
fn explicit_budget_caps_an_unarmed_core_hello() {
    /// The same hello, answered.
    static STEPS: Patience = Patience::measuring("test.hex_client.budget.unarmed_hello_steps");
    /// The hello's refusal at its caller's cap.
    static CAPPED: Patience = Patience::measuring("test.hex_client.budget.unarmed_hello_capped");
    let (cap, answered) = measured(&STEPS, || {
        Core::hello(Shared::try_new(answering(
            agents_hello(),
            Some(MOST_ONE_WAIT),
        ))?)
        .map(drop)
    });
    answered.unwrap();
    let (transport, request) = stalled(Some(cap));
    let shared = Shared::try_new(transport).unwrap();
    let control = triomphe::Arc::clone(&shared);
    let (finished, answer) = mpsc::sync_channel(1);
    let worker = std::thread::spawn(move || {
        let asked = Instant::now();
        let result = Core::hello(control).map(drop);
        let _ = finished.send((asked, Instant::now(), result, armed()));
    });
    // Its send precedes its answer, so it is read once the answer has come.
    let observed = recv_from(&CAPPED, "the capped hello's answer", &answer, &worker);
    let observed_send = request.try_recv();
    // Always release and join the exact owned actor before asserting the original symptom.
    shared.end(Refusal::Broken("owned original-control release".into()));
    worker.join().unwrap();
    let (asked, returned, result, log) = observed.unwrap();
    let sent = observed_send.is_ok();
    told_path("explicit_budget_caps_an_unarmed_core_hello", sent);
    eprintln!(
        "client_budget_control phase=unarmed-hello cap={cap:?} sent={sent} result={result:?} armed={log:?}"
    );
    let at = armed_instant(&log).expect("the hello armed its deadline in its own thread");
    assert!(asked + cap <= at && at <= returned + cap, "{log:?}");
    let waits: &[&str] = if sent {
        &[
            "starting a client operation",
            "sending",
            "the send lock",
            "the answer",
        ]
    } else {
        &["starting a client operation"]
    };
    assert!(armed_at(&log, waits, Some(at)), "{log:?}");
    assert!(
        matches!(&result, Err(Refusal::Os { why, .. }) if why.contains("caller") && !why.contains(LEARNED)),
        "{result:?}"
    );
    assert!(
        returned >= at,
        "refused {:?} before its deadline",
        at.saturating_duration_since(returned)
    );
}

struct TokenLink {
    address: SocketAddr,
    budget: Duration,
    /// Each dial's port and deadline, what the client armed.
    dials: DialDeadlines,
}

impl Link for TokenLink {
    fn lends_identity(&self) -> bool {
        false
    }

    fn reply_budget(&self) -> Option<Duration> {
        Some(self.budget)
    }
    fn dial(&self, port: u32) -> Result<DuplexOwner, Refusal> {
        self.dial_before(port, None)
    }
    fn dial_before(&self, port: u32, deadline: Option<Instant>) -> Result<DuplexOwner, Refusal> {
        self.dials.lock().unwrap().push((port, deadline));
        let left = deadline.map_or(MOST_ONE_WAIT, |deadline| {
            deadline.saturating_duration_since(Instant::now())
        });
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

/// Do complete a bounded real loopback dial, then withhold the token; expect the same caller cap to bound setup, rather
/// than the peer's eventual close: the dial and every wait the connect armed, in its own thread, at its one deadline,
/// and its refusal never before it. The cap is this host's own ([`measured`]: the same connect, its token and its
/// hello's answer written as its dial is accepted); whether the dial came before it is told to the census. The dial is
/// taken once the connect has ended, from the listener's queue, so no accept waits on a dial that never comes.
#[test]
fn explicit_budget_caps_token_after_a_bounded_dial() {
    /// The same connect, answered.
    static STEPS: Patience = Patience::measuring("test.hex_client.budget.token_steps");
    /// The measured connect's dial, accepted.
    static DIALED: Patience = Patience::measuring("test.hex_client.budget.token_dialed");
    /// The connect's refusal at its caller's cap, the token withheld.
    static CAPPED: Patience = Patience::measuring("test.hex_client.budget.token_capped");
    let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
    listener.set_nonblocking(true).unwrap();
    let address = listener.local_addr().unwrap();
    let stop = Arc::new(AtomicBool::new(false));
    let wake = Arc::new(Wake::new().unwrap());
    let (agent_stop, agent_wake) = (Arc::clone(&stop), Arc::clone(&wake));
    let agent = std::thread::spawn(move || -> io::Result<TcpStream> {
        // By the life of the test's stop word, which ends the accept too.
        let mut peer = follow::within(
            &DIALED,
            "the client's dial",
            || {
                if agent_stop.load(Ordering::Acquire) {
                    Party::Ended
                } else {
                    Party::Live
                }
            },
            |by| io_slice(accept_before(&listener, &agent_stop, &agent_wake, by)),
        )
        .map_err(|unmet| io::Error::other(unmet.to_string()))??;
        peer.write_all(&TOKEN.to_le_bytes())?;
        answer_to(&mut peer, 1, agents_hello())?;
        Ok(peer)
    });
    let (cap, connected) = measured(&STEPS, move || {
        guest::own_link(TokenLink {
            address,
            budget: MOST_ONE_WAIT,
            dials: Arc::default(),
        })
        .and_then(guest::Client::connect)
        .map(drop)
    });
    // A connect that never dialed leaves the agent's accept waiting: it is woken first.
    stop.store(true, Ordering::Release);
    wake.wake();
    let measured_peer = agent.join().unwrap();
    connected.unwrap();
    drop(measured_peer.unwrap());
    let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
    listener.set_nonblocking(true).unwrap();
    let dials: DialDeadlines = Arc::default();
    let link = guest::own_link(TokenLink {
        address: listener.local_addr().unwrap(),
        budget: cap,
        dials: Arc::clone(&dials),
    })
    .unwrap();
    let (finished, answer) = mpsc::sync_channel(1);
    let worker = std::thread::spawn(move || {
        let asked = Instant::now();
        let result = guest::Client::connect(link).map(drop);
        let _ = finished.send((asked, Instant::now(), result, armed()));
    });
    let observed = recv_from(&CAPPED, "the capped connect's answer", &answer, &worker);
    // Its dial, if it made one, waits in the listener's queue: one look takes it.
    let peer = accept_before(
        &listener,
        &AtomicBool::new(false),
        &Wake::new().unwrap(),
        Some(Instant::now()),
    );
    let cleanup = peer.as_ref().map(|peer| peer.shutdown(Shutdown::Both));
    worker.join().unwrap();
    let (asked, returned, result, log) = observed.unwrap();
    // Its token's read armed: its dial made before its cap passed.
    let dialed = reached(&log, "reading the relay's token for the connection");
    told_path("explicit_budget_caps_token_after_a_bounded_dial", dialed);
    eprintln!(
        "client_budget_control phase=token cap={cap:?} dialed={dialed} result={result:?} armed={log:?} dials={:?}",
        dials.lock().unwrap()
    );
    assert!(
        matches!(cleanup, Err(_) | Ok(Ok(())))
            || matches!(&cleanup, Ok(Err(error)) if error.kind() == std::io::ErrorKind::NotConnected)
    );
    let at = armed_instant(&log).expect("the connect armed its deadline in its own thread");
    assert!(asked + cap <= at && at <= returned + cap, "{log:?}");
    assert!(
        dials
            .lock()
            .unwrap()
            .iter()
            .all(|dial| *dial == (hex_core::stream::CONTROL_PORT, Some(at)))
    );
    let waits: &[&str] = if dialed {
        &[
            "dialing the guest control connection",
            "reading the relay's token for the connection",
        ]
    } else {
        &["dialing the guest control connection"]
    };
    assert!(armed_at(&log, waits, Some(at)), "{log:?}");
    assert!(matches!(result, Err(Refusal::Os { .. })), "{result:?}");
    assert!(
        returned >= at,
        "refused {:?} before its deadline",
        at.saturating_duration_since(returned)
    );
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
    fn lends_identity(&self) -> bool {
        false
    }

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

/// Do queue an actual API call behind the existing whole-message sender; expect its own cap to end acquisition with no
/// write and no damage to the held sender or connection: every wait it armed, in its own thread, at its one deadline,
/// its refusal the caller's, never before that deadline. The cap is this host's own ([`measured`]: the same list,
/// answered); whether the host let the call reach the send lock before it is told to the census.
#[test]
fn queued_sender_consumes_its_own_budget_without_taking_authority() {
    /// The same list, answered.
    static STEPS: Patience = Patience::measuring("test.hex_client.budget.queued_list_steps");
    /// The queued list's refusal at its own cap, the sender held.
    static CAPPED: Patience = Patience::measuring("test.hex_client.budget.queued_list_capped");
    let (cap, answered) = measured(&STEPS, || {
        configured(answering(no_boxes(), Some(MOST_ONE_WAIT))).list()
    });
    answered.unwrap();
    let (transport, request) = stalled(Some(cap));
    let client = Arc::new(configured(transport));
    let whole = client.conn.shared.sending.lock(None).unwrap().unwrap();
    let control = Arc::clone(&client);
    let (finished, answer) = mpsc::sync_channel(1);
    let worker = std::thread::spawn(move || {
        let asked = Instant::now();
        let result = control.list();
        let _ = finished.send((asked, Instant::now(), result, armed()));
    });
    let result = recv_from(&CAPPED, "the queued list's answer", &answer, &worker);
    // Release the exact owned guard even if the budget assertion fails, then join its waiter.
    drop(whole);
    if result.is_err() {
        client.conn.shared.end(Refusal::Broken(
            "owned queued-sender observation cleanup".into(),
        ));
    }
    worker.join().unwrap();
    let (asked, returned, result, log) = result.unwrap();
    let queued = reached(&log, "the send lock");
    told_path(
        "queued_sender_consumes_its_own_budget_without_taking_authority",
        queued,
    );
    let at = armed_instant(&log).expect("the list armed its deadline in its own thread");
    assert!(asked + cap <= at && at <= returned + cap, "{log:?}");
    let waits: &[&str] = if queued {
        &["starting a client operation", "sending", "the send lock"]
    } else {
        &["starting a client operation"]
    };
    assert!(armed_at(&log, waits, Some(at)), "{log:?}");
    let why = result.unwrap_err();
    assert!(
        matches!(why, Refusal::Os { ref why, .. } if why.contains("caller") && !why.contains(LEARNED))
    );
    assert!(
        returned >= at,
        "refused {:?} before its deadline",
        at.saturating_duration_since(returned)
    );
    assert!(matches!(request.try_recv(), Err(mpsc::TryRecvError::Empty)));
    let state = client.conn.shared.state().unwrap();
    assert!(state.calls.is_empty());
    assert!(state.programs.is_empty());
    assert!(state.closed.is_none());
    assert!(!client.conn.shared.transport.closed.load(Ordering::Acquire));
}

/// Do omit the optional cap on an unarmed kind; expect the legacy wait to remain pending until the exact connection is
/// ended, rather than inheriting another client's cap: its send and its answer's wait armed in its own thread with no
/// deadline at all, so no cap of its own or another's could end it, still waiting once its send went, then refused by
/// the connection's end. Nothing here waits for a time of the test's own.
#[test]
fn default_none_preserves_the_unarmed_wait() {
    /// The uncapped list's send.
    static SENT: Patience = Patience::measuring("test.hex_client.budget.uncapped_list_sent");
    let (transport, request) = stalled(None);
    let client = Arc::new(configured(transport));
    let control = Arc::clone(&client);
    let (finished, answer) = mpsc::sync_channel(1);
    let worker = std::thread::spawn(move || {
        let result = control.list();
        let _ = finished.send((result, armed()));
    });
    let observed_send = recv_on(&SENT, "the request sent", &request);
    // Pending once its send has gone: nothing it armed has an end of its own.
    let before = answer.try_recv();
    client
        .conn
        .shared
        .end(Refusal::Broken("owned legacy-wait release".into()));
    worker.join().unwrap();
    observed_send.unwrap();
    assert!(
        matches!(before, Err(mpsc::TryRecvError::Empty)),
        "{before:?}"
    );
    let (result, log) = answer.try_recv().unwrap();
    assert!(matches!(result, Err(Refusal::Broken(_))), "{result:?}");
    assert!(armed_at(&log, &["sending", "the answer"], None), "{log:?}");
}

/// A transport whose answers' learned waits are its own, the long one's or the short one's, each measuring (decision 9;
/// Ada's rule of 2026-11-13: no record seeds them): it answers a call as it is asked where it is given an answer
/// ([`BootstrapProbe`]), else never.
struct Calibrated<const LONG: bool>(BootstrapProbe);
impl<const LONG: bool> Transport for Calibrated<LONG> {
    type Reading = ();
    fn answers() -> &'static [Patience; KINDS] {
        static LONG_WAIT: [Patience; KINDS] =
            [const { Patience::measuring("test.client_budget.long") }; KINDS];
        static SHORT: [Patience; KINDS] =
            [const { Patience::measuring("test.client_budget.short") }; KINDS];
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

/// A [`Calibrated`] transport giving each call `budget`, answering one with `reply` where given, and its first send's
/// notice.
fn calibrated<const LONG: bool>(
    reply: Option<Answer>,
    budget: Option<Duration>,
) -> (Calibrated<LONG>, mpsc::Receiver<()>) {
    let (io, sent) = stalled(None);
    (
        Calibrated(BootstrapProbe {
            io: Offers::unobserved(io),
            reply: Mutex::new(reply),
            budget,
        }),
        sent,
    )
}

/// The short learned wait's W* for `request`'s kind (each kind learns its own, [`kind_of`]): where it has none, its cold
/// start, handled and told (decision 9: no record seeds it): `request` answered with `reply` as it is asked is its first
/// answer, the sample its W* starts from.
fn short_learned(request: &Request, reply: Answer) -> Duration {
    let learned = || kind_of(Calibrated::<false>::answers(), request).and_then(Patience::deadline);
    if let Some(wait) = learned() {
        eprintln!(
            "client_budget_learned wait=short request={request:?} cold_start=false w_star={wait:?}"
        );
        return wait;
    }
    let (transport, _) = calibrated::<false>(Some(reply), None);
    let taught = configured(transport).conn.call_before(
        wire::MAX_MESSAGE,
        request,
        |_, _| Ok(NONE),
        None,
        None,
    );
    assert!(taught.is_ok(), "{taught:?}");
    let wait = learned().expect("an answered call teaches its kind's learned wait");
    eprintln!(
        "client_budget_learned wait=short request={request:?} cold_start=true w_star={wait:?}"
    );
    wait
}

/// The hello a stand-in agent answers, of this build.
fn agents_hello() -> Answer {
    Answer::Hello(wire::Hello {
        build: wire::build(),
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
    })
}

/// What a capped list did: when it was asked and returned, its refusal, what it armed in its own thread, and whether its
/// request went.
struct Timed {
    asked: Instant,
    returned: Instant,
    refusal: Refusal,
    armed: Vec<Armed>,
    sent: bool,
}

fn timed_call<T: Transport>(transport: T, request: mpsc::Receiver<()>) -> Timed {
    /// The list's refusal at its caller's cap.
    static CAPPED: Patience = Patience::measuring("test.hex_client.budget.timed_list_capped");
    let client = Arc::new(configured(transport));
    let control = Arc::clone(&client);
    let (finished, answer) = mpsc::sync_channel(1);
    let worker = std::thread::spawn(move || {
        let asked = Instant::now();
        let result = control.list();
        let _ = finished.send((asked, Instant::now(), result, armed()));
    });
    // Its send precedes its answer, so it is read once the answer has come.
    let result = recv_from(&CAPPED, "the capped list's answer", &answer, &worker);
    let observed_send = request.try_recv();
    client
        .conn
        .shared
        .end(Refusal::Broken("owned calibrated control release".into()));
    worker.join().unwrap();
    let (asked, returned, result, armed) = result.unwrap();
    Timed {
        asked,
        returned,
        refusal: result.unwrap_err(),
        armed,
        sent: observed_send.is_ok(),
    }
}

/// A capped list's verdict, whichever path the host gave it: every wait it armed, in its own thread, at its one deadline
/// (its ask and `budget`), the refusal the caller's expiry, never before that deadline. Returns that deadline.
fn caller_expired(timed: &Timed, budget: Duration) -> Instant {
    let at = armed_instant(&timed.armed).expect("the list armed its deadline in its own thread");
    assert!(
        timed.asked + budget <= at && at <= timed.returned + budget,
        "{:?}",
        timed.armed
    );
    let waits: &[&str] = if timed.sent {
        &["starting a client operation", "sending", "the answer"]
    } else {
        &["starting a client operation"]
    };
    assert!(armed_at(&timed.armed, waits, Some(at)), "{:?}", timed.armed);
    assert!(
        matches!(&timed.refusal, Refusal::Os { why, .. } if why.contains("caller") && !why.contains(LEARNED)),
        "{:?}",
        timed.refusal
    );
    assert!(
        timed.returned >= at,
        "refused {:?} before its deadline",
        at.saturating_duration_since(timed.returned)
    );
    at
}

/// Do exhaust a caller's bound in actual Core calls, once with no learned wait under it (the long
/// one's cold start: nothing answered yet) and once with one learned from an answered list, ending
/// before it; expect each refused as the caller's expiry, every wait at its one deadline, never
/// before it; the learned wait before it a run-out told to the census and never the call's end
/// (AC-T7.4.1), and the caller's expiry teaching no learned wait anything. Each cap is this host's
/// own ([`measured`]); the short one's is its learned wait's W* and that, strictly past the bound.
/// The census runs in a fresh exact-test child so it never competes with the existing
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
    /// The same list, answered.
    static STEPS: Patience = Patience::measuring("test.hex_client.budget.timeout_cause_steps");
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
    let (cap, answered) = measured(&STEPS, || {
        configured(answering(no_boxes(), Some(MOST_ONE_WAIT))).list()
    });
    answered.unwrap();
    let unlearned = || {
        Calibrated::<true>::answers()
            .iter()
            .all(|wait| wait.deadline().is_none())
    };
    // The long wait's cold start: it has seen no answer, so the caller's cap alone bounds the call.
    assert!(unlearned());
    eprintln!("client_budget_learned wait=long cold_start=true");
    let (transport, request) = calibrated::<true>(None, Some(cap));
    let long = timed_call(transport, request);
    told_path(
        "timeout_cause_and_learning_keep_caller_authority_separate",
        long.sent,
    );
    caller_expired(&long, cap);
    assert!(
        long.armed.iter().all(|armed| !armed.learned),
        "{:?}",
        long.armed
    );
    assert_eq!(
        SAMPLES.load(Ordering::SeqCst),
        0,
        "caller expiry cannot calibrate a learned bound"
    );
    assert!(unlearned(), "a caller's expiry taught the long wait");
    // The short wait's cold start, handled and told: its first answer, the census's first sample.
    let short = short_learned(&Request::Call(Call::List), no_boxes());
    assert_eq!(SAMPLES.load(Ordering::SeqCst), 1);
    let budget = short + cap;
    let (transport, request) = calibrated::<false>(None, Some(budget));
    let learned = timed_call(transport, request);
    let at = caller_expired(&learned, budget);
    // Its learned wait, where the call armed it under its caller's deadline, earlier: a run-out told, never its end.
    let bound = learned
        .armed
        .iter()
        .find(|armed| armed.learned)
        .and_then(|armed| armed.at);
    told_path(
        "timeout_cause_and_learning_keep_caller_authority_separate",
        bound.is_some(),
    );
    assert!(bound.is_none_or(|bound| bound < at), "{:?}", learned.armed);
    let run_outs = RUN_OUT.load(Ordering::SeqCst);
    assert!(run_outs <= 1);
    assert_eq!(SAMPLES.load(Ordering::SeqCst), 1 + run_outs);
    if bound.is_some() {
        assert_eq!(run_outs, 1);
    }
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

/// A stand-in agent busy past what it has answered in before, as the guest's node was initializing its GPU: it answers a
/// hello only once [`LATE_DUE`] is set, by the test for a first hello (the learned wait's cold start, its first answer)
/// and then by [`late_census`], where it is installed, as the hello's learned wait, the slowest answer seen (decision 9;
/// no record seeds it), runs out. `CALLER` names its waits apart.
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
        static UNDER_CALLER: [Patience; KINDS] =
            [const { Patience::measuring("test.client_budget.late.caller") }; KINDS];
        static ALONE: [Patience; KINDS] =
            [const { Patience::measuring("test.client_budget.late.alone") }; KINDS];
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

/// The hello's learned wait's cold start, handled and told: it has seen no answer, so the agent answers a first hello
/// as it is asked ([`LATE_DUE`] set), its time the sample the wait starts from; `cap` is that hello's caller's deadline,
/// none for none. Returns the hello's client, kept so its connection stays, and the wait's W*: that first answer.
fn taught_hello<const CALLER: bool>(
    shared: &SharedOwner<LateHello<CALLER>>,
    cap: Option<ReplyDeadline>,
    name: &'static str,
) -> (Core<LateHello<CALLER>>, Duration) {
    LATE_DUE.store(true, Ordering::SeqCst);
    // Its answer is there to read: the agent's connection woken, as a run-out wakes it.
    LATE_WAKE.get().unwrap().send(&[1]).unwrap();
    let client = Core::hello_before(triomphe::Arc::clone(shared), cap).unwrap();
    LATE_DUE.store(false, Ordering::SeqCst);
    let answered = LATE_ANSWERED.lock().unwrap().clone();
    let [(taught, first)] = answered.as_slice() else {
        panic!("one first answer told: {answered:?}");
    };
    assert_eq!(*taught, name);
    eprintln!(
        "client_late_answer cold_start=true wait={name} taught_by=a_first_hello w_star={first:?}"
    );
    (client, *first)
}

/// AC-T7.4.1 (original): on 00aad a Firecracker guest with the AMD GPU passed through booted, and its relay's hello to
/// its own node, busy with the GPU's initialization, was refused, "none came in 2.354074ms, twice the slowest answer to
/// a call of its kind", while the boot had most of its 60 s left: the guest panicked. A learned bound is calibration,
/// not the caller's authority. Do: say hello under a caller's absolute deadline to an agent that answers only once the
/// hello's learned wait (the slowest answer it has seen: a first hello's, its cold start; decision 9) has run out.
/// Expect: the hello answered; its run-out told to the census once, at the learned bound; its answer told too, later
/// than that bound.
#[test]
fn original_a_late_answer_inside_its_callers_deadline_is_taken_its_run_out_recorded() {
    if !in_late_census_child(
        "calls::budget_tests::original_a_late_answer_inside_its_callers_deadline_is_taken_its_run_out_recorded",
    ) {
        return;
    }
    const NAME: &str = "test.client_budget.late.caller";
    let shared = Shared::try_new(late_hello::<true>()).unwrap();
    // A caller's deadline no answer here comes near: the late answer is there to read as the learned wait runs out.
    let cap = ReplyDeadline::caller_before(answer_by(None)).unwrap();
    let (_taught, bound) = taught_hello(&shared, Some(cap), NAME);
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
    assert_eq!(ran_out, [(NAME, bound)]);
    assert!(
        matches!(answered.as_slice(), [(NAME, first), (NAME, took)] if *first == bound && *took > bound),
        "{answered:?}"
    );
}

/// AC-T7.4.1, decision 9 (original; the person's ruling of 2026-11-13: W* is a bound only while answers come within it):
/// a call with no caller's deadline was refused at its kind's learned wait, "none came in 1ms, the slowest answer to a
/// call of its kind", so a fresh process's second call was refused whenever it was slower than its first. Do: say hello
/// with no caller deadline to an agent that answers only once the hello's learned wait (a first hello's answer, its cold
/// start) has run out. Expect: the hello answered, the call following its agent past W*; its run-out told to the census
/// once, at the learned bound; its answer told too, later than that bound.
#[test]
fn original_with_no_callers_deadline_a_call_follows_its_agent_past_the_learned_wait() {
    if !in_late_census_child(
        "calls::budget_tests::original_with_no_callers_deadline_a_call_follows_its_agent_past_the_learned_wait",
    ) {
        return;
    }
    const NAME: &str = "test.client_budget.late.alone";
    let shared = Shared::try_new(late_hello::<false>()).unwrap();
    let (_taught, bound) = taught_hello(&shared, None, NAME);
    let result = Core::hello_before(shared, None).map(drop);
    let ran_out = LATE_RAN_OUT.lock().unwrap().clone();
    let answered = LATE_ANSWERED.lock().unwrap().clone();
    eprintln!(
        "client_late_answer under=none result={result:?} ran_out={ran_out:?} answered={answered:?}"
    );
    assert!(
        result.is_ok(),
        "a call with no caller's deadline was refused at its learned wait: {result:?}"
    );
    assert_eq!(ran_out, [(NAME, bound)]);
    assert!(
        matches!(answered.as_slice(), [(NAME, first), (NAME, took)] if *first == bound && *took > bound),
        "{answered:?}"
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

/// One read the client made of its control connection: the bytes it brought, or why none came.
#[derive(Clone, Copy, Debug)]
struct TokenRead {
    result: Result<usize, io::ErrorKind>,
}

/// The client's control connection in the partial-token test: each read it makes, one byte at a time, recorded in
/// order; and the token's second byte sent to it, through its peer's half held here, as the client, having read the
/// first, finds no more and waits again: in that very read, so the byte comes on that fact, in the client's own turn,
/// with no other thread's inside its deadline.
struct ReadNotice {
    socket: TcpStream,
    peer: TcpStream,
    second: Arc<Mutex<Option<u8>>>,
    reads: Arc<Mutex<Vec<TokenRead>>>,
}

impl Read for ReadNotice {
    fn read(&mut self, bytes: &mut [u8]) -> io::Result<usize> {
        let want = bytes.len().min(1);
        let result = self.socket.read(&mut bytes[..want]);
        let read = TokenRead {
            result: result.as_ref().copied().map_err(io::Error::kind),
        };
        let mut reads = self.reads.lock().unwrap();
        let progressed = reads
            .iter()
            .any(|read| matches!(read.result, Ok(n) if n > 0));
        reads.push(read);
        if progressed
            && matches!(read.result, Err(io::ErrorKind::WouldBlock))
            && let Some(byte) = self.second.lock().unwrap().take()
        {
            (&self.peer).write_all(&[byte])?;
        }
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
            peer: self.peer.try_clone()?,
            second: Arc::clone(&self.second),
            reads: Arc::clone(&self.reads),
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

/// What a client set on one of its connections, in order: its blocking mode and its timeouts.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Set {
    Nonblocking(bool),
    ReadTimeout(Option<Duration>),
    WriteTimeout(Option<Duration>),
}

/// What a client set on one connection, in order, shared by its clones and the link that dialed it.
type SetLog = Arc<Mutex<Vec<Set>>>;

/// A connection whose every option the client sets is recorded, its clones' with it: what the client armed on it.
struct Watched {
    socket: TcpStream,
    set: SetLog,
}

impl Read for Watched {
    fn read(&mut self, bytes: &mut [u8]) -> io::Result<usize> {
        self.socket.read(bytes)
    }
}

impl Write for Watched {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        self.socket.write(bytes)
    }
    fn flush(&mut self) -> io::Result<()> {
        self.socket.flush()
    }
}

impl Duplex for Watched {
    fn try_clone(&self) -> io::Result<DuplexOwner> {
        own_duplex(Self {
            socket: self.socket.try_clone()?,
            set: Arc::clone(&self.set),
        })
        .map_err(|_| io::Error::from(io::ErrorKind::OutOfMemory))
    }
    fn shutdown(&self, how: Shutdown) -> io::Result<()> {
        self.socket.shutdown(how)
    }
    fn set_write_timeout(&self, timeout: Option<Duration>) -> io::Result<()> {
        self.set.lock().unwrap().push(Set::WriteTimeout(timeout));
        self.socket.set_write_timeout(timeout)
    }
    fn set_read_timeout(&self, timeout: Option<Duration>) -> io::Result<()> {
        self.set.lock().unwrap().push(Set::ReadTimeout(timeout));
        self.socket.set_read_timeout(timeout)
    }
    fn handle(&self) -> Handle<'_> {
        #[cfg(unix)]
        return std::os::fd::AsFd::as_fd(&self.socket);
        #[cfg(windows)]
        std::os::windows::io::AsSocket::as_socket(&self.socket)
    }
    fn set_nonblocking(&self, on: bool) -> io::Result<()> {
        self.set.lock().unwrap().push(Set::Nonblocking(on));
        self.socket.set_nonblocking(on)
    }
}

/// Whether what a client set on a connection leaves no timer on it: no timeout set, and blocking where it last chose.
fn untimed(set: &[Set]) -> bool {
    set.iter()
        .all(|set| !matches!(set, Set::ReadTimeout(Some(_)) | Set::WriteTimeout(Some(_))))
        && set
            .iter()
            .rev()
            .find_map(|set| match set {
                Set::Nonblocking(on) => Some(!on),
                _ => None,
            })
            .unwrap_or(true)
}

/// A guest link whose control connection was made before its dial, its peer the test's, and whose stdio dials connect
/// to `stdio`, where the kernel queues each for its accept: no thread of the test's runs within a dial. It records each
/// dial's port and deadline, and what the client set on each stdio connection: what the client armed.
struct RecordedLink {
    control: Mutex<Option<DuplexOwner>>,
    stdio: Option<SocketAddr>,
    budget: Option<Duration>,
    deadlines: DialDeadlines,
    sets: Arc<Mutex<Vec<SetLog>>>,
}

impl Link for RecordedLink {
    fn lends_identity(&self) -> bool {
        false
    }

    fn reply_budget(&self) -> Option<Duration> {
        self.budget
    }
    fn dial(&self, port: u32) -> Result<DuplexOwner, Refusal> {
        self.dial_before(port, None)
    }
    fn dial_before(&self, port: u32, deadline: Option<Instant>) -> Result<DuplexOwner, Refusal> {
        self.deadlines.lock().unwrap().push((port, deadline));
        if port == hex_core::stream::CONTROL_PORT {
            return self.control.lock().unwrap().take().ok_or_else(|| {
                Refusal::Broken("the recorded link's control connection was dialed twice".into())
            });
        }
        let address = self
            .stdio
            .ok_or_else(|| Refusal::Broken("the recorded link dials no stdio".into()))?;
        let socket = match deadline {
            Some(at) => {
                TcpStream::connect_timeout(&address, at.saturating_duration_since(Instant::now()))
            }
            None => TcpStream::connect(address),
        }
        .map_err(|error| Refusal::os("recorded loopback dial", error))?;
        // These controls deliberately send individual bytes/small frames; TCP coalescing is
        // unrelated to whether protocol progress consumes one deadline.
        socket.set_nodelay(true).unwrap();
        let set = Arc::new(Mutex::new(Vec::new()));
        self.sets.lock().unwrap().push(Arc::clone(&set));
        own_duplex(Watched { socket, set })
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

/// Do make real partial token progress before the original deadline, each byte sent on a fact, never at a time of the
/// test's own: the first before the client dials, the second as the client, having read the first, waits again, in that
/// read itself ([`ReadNotice`]); expect every wait the connect armed, in its own thread, at its one deadline, its
/// progress never re-arming it, and its expiry naming its caller, never before that deadline. The cap is this host's own
/// ([`measured`]: the same connect, its whole token and its hello's answer there to read); whether the host let both
/// reads come before it is told to the census.
#[test]
fn partial_token_progress_does_not_reset_the_absolute_deadline() {
    /// The same connect, answered.
    static STEPS: Patience = Patience::measuring("test.hex_client.budget.partial_token_steps");
    /// The connect's refusal at its original deadline, the token cut short.
    static EXPIRED: Patience = Patience::measuring("test.hex_client.budget.partial_token_expired");
    let (socket, peer) = control_pair(vec![(1, agents_hello())]);
    let link = guest::own_link(RecordedLink {
        control: Mutex::new(Some(own_duplex(socket).unwrap())),
        stdio: None,
        budget: Some(MOST_ONE_WAIT),
        deadlines: Arc::default(),
        sets: Arc::default(),
    })
    .unwrap();
    let (cap, connected) = measured(&STEPS, move || guest::Client::connect(link).map(drop));
    drop(peer);
    connected.unwrap();
    let token = TOKEN.to_le_bytes();
    let (socket, mut peer) = absolute_pair();
    peer.write_all(&token[..1]).unwrap();
    let reads = Arc::new(Mutex::new(Vec::new()));
    let recorded: DialDeadlines = Arc::default();
    let link = guest::own_link(RecordedLink {
        control: Mutex::new(Some(
            own_duplex(ReadNotice {
                socket,
                peer: peer.try_clone().unwrap(),
                second: Arc::new(Mutex::new(Some(token[1]))),
                reads: Arc::clone(&reads),
            })
            .unwrap(),
        )),
        stdio: None,
        budget: Some(cap),
        deadlines: Arc::clone(&recorded),
        sets: Arc::default(),
    })
    .unwrap();
    let (finished, answer) = mpsc::sync_channel(1);
    let worker = std::thread::spawn(move || {
        let result = guest::Client::connect(link).map(drop);
        let _ = finished.send((Instant::now(), result, armed()));
    });
    let completed = recv_from(&EXPIRED, "the connect's refusal", &answer, &worker);
    // Every observation is taken before any assertion: the peer closed, the worker joined.
    let cleanup = peer.shutdown(Shutdown::Both);
    drop(peer);
    worker.join().unwrap();
    let reads = reads.lock().unwrap().clone();
    let deadlines = recorded.lock().unwrap().clone();
    let (returned, result, log) = completed.unwrap();
    let deadline = armed_instant(&log).expect("the connect armed its deadline in its own thread");
    // The reads that brought a byte, by their place among all its reads: the token's first and second, where both came.
    let progress: Vec<usize> = reads
        .iter()
        .enumerate()
        .filter(|(_, read)| matches!(read.result, Ok(n) if n > 0))
        .map(|(at, _)| at)
        .collect();
    let both = progress.len() == 2;
    told_path(
        "partial_token_progress_does_not_reset_the_absolute_deadline",
        both,
    );
    eprintln!(
        "client_budget_token cap={cap:?} deadlines={deadlines:?} actual_reads={reads:?} client={result:?} armed={log:?}"
    );
    assert!(
        cleanup.is_ok()
            || matches!(&cleanup, Err(error) if error.kind() == io::ErrorKind::NotConnected)
    );
    // What the client armed: its dial, where it made one, and every wait of the token's read at the one deadline.
    assert!(
        deadlines
            .iter()
            .all(|dial| *dial == (hex_core::stream::CONTROL_PORT, Some(deadline))),
        "{deadlines:?}"
    );
    let waits: &[&str] = if deadlines.is_empty() {
        &["dialing the guest control connection"]
    } else {
        &[
            "dialing the guest control connection",
            "reading the relay's token for the connection",
        ]
    };
    assert!(armed_at(&log, waits, Some(deadline)), "{log:?}");
    if let [first, second] = progress.as_slice() {
        // The second byte came only as the client, having read the first, found no more and read again; the rest
        // never come, so the token's read waited on at the one deadline and was refused by it.
        assert!(
            reads[*first + 1..*second]
                .iter()
                .any(|read| matches!(read.result, Err(io::ErrorKind::WouldBlock))),
            "the client waited again between its two reads: {reads:?}"
        );
        assert!(
            matches!(
                &result,
                Err(Refusal::Os {
                    op: "reading the relay's token for the connection",
                    ..
                })
            ),
            "{result:?}"
        );
    }
    assert!(
        matches!(&result, Err(Refusal::Os { why, .. }) if why.contains("caller")),
        "{result:?}"
    );
    // Refused at its original deadline, never before it.
    assert!(
        returned >= deadline,
        "refused {:?} before its deadline",
        deadline.saturating_duration_since(returned)
    );
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

enum StreamPhase {
    WriteLate,
    Stop,
}

/// Every stream an exec can ask for.
fn all_streams() -> Stdio {
    Stdio {
        ask: hex_api::Ask {
            stdin: true,
            stdout: true,
            stderr: true,
        },
        ..Stdio::null()
    }
}

/// Do ask for all three streams in one actual guest exec, then start the box, read the program's stdout, exec again on
/// the same connection asking stdin alone, and delete the box; expect, from what the client armed and no clock: the
/// exec's three named dials, and every wait it armed in its own thread, at one deadline, the exec's own cap (its ask's
/// instant and the budget); the second exec's dial at its own, later; the start's waits at its own; no timeout and
/// blocking at hand-off on every named stream, and no timeout on the connection: no workload or connection timer. And
/// the program pending after its exec, its stdout carrying bytes written after it, both programs ending Exited(0).
/// The budget is MOST_ONE_WAIT, poll(2)'s int-millisecond ceiling (an API's own bound, the longest one wait the client
/// arms), so no step here races it (ROOT, 2026-11-14). Every answer a call reads is written before it asks; the agent's
/// side reads each request after.
#[test]
fn named_streams_share_one_operation_cap_without_a_workload_or_connection_timer() {
    use hex_core::stream::{self, Named};
    /// The exec's three named dials, accepted and each named.
    static STREAMS_DIALED: Patience =
        Patience::measuring("test.hex_client.budget.named_streams_dialed");
    /// The late stdout bytes, read through the exec's own stream.
    static LATE_READ: Patience = Patience::measuring("test.hex_client.budget.named_late_read");
    /// Each request the client sent, read by the agent's side.
    static ASKED: Patience = Patience::measuring("test.hex_client.budget.named_asked");
    /// The agent's own end, once the last client has gone.
    static AGENT_ENDED: Patience = Patience::measuring("test.hex_client.budget.named_agent_ended");
    let budget = MOST_ONE_WAIT;
    let stdio = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
    stdio.set_nonblocking(true).unwrap();
    let (socket, mut peer) = control_pair(vec![(1, Answer::Started { pid: 7 })]);
    let control_set = Arc::new(Mutex::new(Vec::new()));
    let recorded: DialDeadlines = Arc::default();
    let sets = Arc::default();
    let link = guest::own_link(RecordedLink {
        control: Mutex::new(Some(
            own_duplex(Watched {
                socket,
                set: Arc::clone(&control_set),
            })
            .unwrap(),
        )),
        stdio: Some(stdio.local_addr().unwrap()),
        budget: Some(budget),
        deadlines: Arc::clone(&recorded),
        sets: Arc::clone(&sets),
    })
    .unwrap();
    // The agent's side: each request the client sent, read in turn, then the connection's end.
    let mut agent_peer = peer.try_clone().unwrap();
    let agent = std::thread::spawn(move || -> io::Result<(Vec<Request>, usize)> {
        let mut asked = Vec::new();
        for _ in 0..4 {
            // A read that can't be cut: it ends at the client's request or at the connection's end.
            asked.push(
                follow::whole(&ASKED, "the client's next request", || {
                    next_request(&mut agent_peer)
                })?
                .1,
            );
        }
        let end = follow::whole(&AGENT_ENDED, "the connection's end", || {
            agent_peer.read(&mut [0])
        })?;
        Ok((asked, end))
    });
    let stop = Arc::new(AtomicBool::new(false));
    let wake = Arc::new(Wake::new().unwrap());
    let owned = Arc::new(Mutex::new(Vec::<TcpStream>::new()));
    // One WriteLate and then Stop; neither idle phase is a workload lifetime timer.
    let (late, stream_phase) = mpsc::sync_channel(2);
    let (streams_stop, streams_wake, streams_owned) =
        (Arc::clone(&stop), Arc::clone(&wake), Arc::clone(&owned));
    let streams = std::thread::spawn(move || -> io::Result<Vec<Named>> {
        // The first exec's three dials, one fact: one wait for all three accepted and named.
        // One wait that can't be cut: three accepts and their names, made whole, each ending at its stop word.
        let by = None;
        let mut held = follow::whole(
            &STREAMS_DIALED,
            "the first exec's three dials",
            || -> io::Result<Vec<_>> {
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
            },
        )?;
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
        let _ = armed();
        let asked = Instant::now();
        let transport = guest::Stream::dial(link)?;
        let dialed = (asked, Instant::now(), armed());
        let client = configured(transport);
        let id = BoxId::new("budget-unit")?;
        let asked = Instant::now();
        let mut proc = client.exec(&id, Program::of(&["cat"]), all_streams())?;
        let execed = (asked, Instant::now(), armed());
        let pending = proc.try_wait()?;
        let _ = armed();
        answer_to(&mut peer, 2, Answer::Done)
            .map_err(|error| Refusal::os("answering the start before it is asked", error))?;
        let asked = Instant::now();
        client.start(&id)?;
        let started = (asked, Instant::now(), armed());
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
        let bytes = recv_on(&LATE_READ, "the late fixture's bytes", &bytes)
            .map_err(|error| Refusal::os("observing requested late fixture bytes", error))?
            .map_err(|error| Refusal::os("reading late fixture stdio", error))?;
        // A second exec on the same connection, after the first's setup: its own cap.
        answer_to(&mut peer, 3, Answer::Started { pid: 8 })
            .map_err(|error| Refusal::os("answering the second exec before it is asked", error))?;
        let _ = armed();
        let asked = Instant::now();
        let mut second = client.exec(
            &id,
            Program::of(&["cat"]),
            Stdio {
                ask: hex_api::Ask {
                    stdin: true,
                    ..hex_api::Ask::default()
                },
                ..Stdio::null()
            },
        )?;
        let execed_again = (asked, Instant::now(), armed());
        answer_to(&mut peer, 4, Answer::Done)
            .and_then(|()| answer_to(&mut peer, 1, Answer::Exited(Exit::Code(0))))
            .and_then(|()| answer_to(&mut peer, 3, Answer::Exited(Exit::Code(0))))
            .map_err(|error| Refusal::os("answering the delete before it is asked", error))?;
        client.delete(&id)?;
        // A program's end through a Proc is followed to it, as the suites follow one.
        let ended = (
            hex_api::conformance::end_of(&mut proc)?,
            hex_api::conformance::end_of(&mut second)?,
        );
        drop((proc, second));
        let clean = {
            let state = client.conn.shared.state()?;
            state.calls.is_empty() && state.programs.is_empty() && state.given.is_empty()
        };
        drop(client);
        Ok((
            dialed,
            execed,
            started,
            execed_again,
            pending,
            bytes,
            ended,
            clean,
        ))
    })();
    let _ = late.send(StreamPhase::Stop);
    // A failed setup can leave an accept waiting on a dial that won't come, and the agent's side on a request that
    // won't: wake the accepts, and end the connection and the streams, before waiting on either.
    stop.store(true, Ordering::Release);
    wake.wake();
    if result.is_err() {
        let _ = peer.shutdown(Shutdown::Both);
        for owned in owned.lock().unwrap().iter() {
            let _ = owned.shutdown(Shutdown::Both);
        }
    }
    drop(peer);
    let names = streams.join().unwrap();
    let agent_result = agent.join().unwrap();
    if let Some(reading) = reading {
        reading.join().unwrap();
    }
    let deadlines = recorded.lock().unwrap().clone();
    let sets: Vec<Vec<Set>> = sets
        .lock()
        .unwrap()
        .iter()
        .map(|set| set.lock().unwrap().clone())
        .collect();
    let control_set = control_set.lock().unwrap().clone();
    eprintln!(
        "client_budget_named result={result:?} agent={agent_result:?} deadlines={deadlines:?} sets={sets:?} control={control_set:?} named_count={:?}",
        names.as_ref().map(Vec::len)
    );
    // No assertion can strand either owned accept/read actor.
    let (dialed, execed, started, execed_again, pending, bytes, ended, clean) = result.unwrap();
    let (asked, end) = agent_result.unwrap();
    let names = names.unwrap();
    // Each operation's one deadline, from its own thread's log: its ask's instant and the budget.
    let deadline_of = |(asked, returned, log): &(Instant, Instant, Vec<Armed>), waits: &[&str]| {
        let at = armed_instant(log).expect("the operation armed its deadline in this thread");
        assert!(*asked + budget <= at && at <= *returned + budget, "{log:?}");
        assert!(armed_at(log, waits, Some(at)), "{log:?}");
        at
    };
    let connection_cap = deadline_of(
        &dialed,
        &[
            "dialing the guest control connection",
            "reading the relay's token for the connection",
        ],
    );
    let exec_cap = deadline_of(
        &execed,
        &[
            "starting a client operation",
            "dialing a stdio connection",
            "naming a stdio connection",
            "sending",
            "the answer",
        ],
    );
    let start_cap = deadline_of(&started, &["starting a client operation", "the answer"]);
    let second_cap = deadline_of(
        &execed_again,
        &[
            "starting a client operation",
            "dialing a stdio connection",
            "the answer",
        ],
    );
    assert!(
        connection_cap < exec_cap && exec_cap < start_cap && start_cap < second_cap,
        "each operation armed its own cap"
    );
    assert_eq!(
        deadlines,
        [
            (stream::CONTROL_PORT, Some(connection_cap)),
            (stream::STDIO_PORT, Some(exec_cap)),
            (stream::STDIO_PORT, Some(exec_cap)),
            (stream::STDIO_PORT, Some(exec_cap)),
            (stream::STDIO_PORT, Some(second_cap)),
        ],
        "the exec's three named dials share its one cap, the second exec's its own"
    );
    // No workload or connection timer: nothing timed on any named stream nor on the connection.
    assert_eq!(sets.len(), 4);
    assert!(sets.iter().all(|set| untimed(set)), "{sets:?}");
    assert!(
        control_set
            .iter()
            .all(|set| !matches!(set, Set::ReadTimeout(Some(_)) | Set::WriteTimeout(Some(_)))),
        "{control_set:?}"
    );
    assert_eq!(pending, None);
    assert_eq!(&bytes, b"late");
    assert_eq!(ended, (Exit::Code(0), Exit::Code(0)));
    assert!(clean);
    assert!(
        matches!(
            asked.as_slice(),
            [
                Request::Call(Call::Exec {
                    streams: Streams {
                        stdin: true,
                        stdout: true,
                        stderr: true
                    },
                    ..
                }),
                Request::Call(Call::Start(_)),
                Request::Call(Call::Exec {
                    streams: Streams {
                        stdin: true,
                        stdout: false,
                        stderr: false
                    },
                    ..
                }),
                Request::Call(Call::Delete(_))
            ]
        ),
        "{asked:?}"
    );
    // Completed Procs send no Release; dropping the last client closed the connection.
    assert_eq!(end, 0);
    assert_eq!(names.len(), 3);
    assert!(
        names
            .iter()
            .all(|name| name.exec.conn == TOKEN && name.exec.id == 1)
    );
}

/// What the offered-expiry control sees happen: its open begin, and its exec end: when it was asked and returned, how it
/// ended, and what it armed in its own thread.
enum Offered {
    Opening,
    Ended(Instant, Instant, Result<(), Refusal>, Vec<Armed>),
}

struct Offers {
    io: Stalled,
    inbound: Mutex<VecDeque<Vec<u8>>>,
    outbound: Mutex<Vec<(u64, Request)>>,
    seen: mpsc::Sender<Offered>,
    /// The open holds until the sibling call has completed, a fact the test sends.
    sibling: Mutex<mpsc::Receiver<()>>,
    program: Mutex<Option<Stdio>>,
    /// Whether an open holds; one that doesn't is refused at once, the steps to it all a measure of it takes.
    hold: bool,
    /// The deadline the open was given: what the client armed it with.
    opened: Mutex<Option<ReplyDeadline>>,
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
            hold: true,
            opened: Mutex::new(None),
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
        self.io.budget
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
                    setup: None,
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
        *self.opened.lock().unwrap() = Some(deadline);
        if !self.hold {
            return Err(Refusal::Broken(
                "a measured open, refused as it begins".into(),
            ));
        }
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

/// The ask of an exec or a run that asks for its stdin alone.
fn stdin_alone() -> Stdio {
    Stdio {
        ask: hex_api::Ask {
            stdin: true,
            stdout: false,
            stderr: false,
        },
        ..Stdio::null()
    }
}

/// Do stall one bounded offered-stream open until a sibling call has completed, then past its deadline; expect the
/// sibling answered, the open's caller expiry, a same-id Release, its made ends closed and no call/program/given
/// reservation left. Each step waits on the fact before it, never on an observation window (AC-T7.4.1). Every wait the
/// exec armed, in its own thread, and its open's deadline, at the exec's one deadline; its refusal never before it. The
/// cap is this host's own ([`measured`]: the same exec, its open refused as it begins); whether the host let the open
/// begin before it is told to the census.
#[test]
fn offered_expiry_releases_only_its_ownership_while_a_sibling_progresses() {
    /// The same exec, its open refused as it begins.
    static STEPS: Patience = Patience::measuring("test.hex_client.budget.offered_steps");
    /// The worker's first event: its open begun, or its exec ended without one.
    static FIRST: Patience = Patience::measuring("test.hex_client.budget.offered_first");
    /// The exec's end, its open refused at the caller's expiry.
    static ENDED: Patience = Patience::measuring("test.hex_client.budget.offered_ended");
    let (cap, measured_exec) = measured(&STEPS, || {
        let (io, _) = stalled(Some(MOST_ONE_WAIT));
        configured(Offers {
            hold: false,
            ..Offers::unobserved(io)
        })
        .exec(
            &BoxId::new("offered-budget-unit")?,
            Program::of(&["cat"]),
            stdin_alone(),
        )
        .map(drop)
    });
    assert!(
        matches!(measured_exec, Err(Refusal::Broken(_))),
        "{measured_exec:?}"
    );
    let (io, _) = stalled(Some(cap));
    let (seen, events) = mpsc::channel();
    let (sibling_done, sibling_wait) = mpsc::channel();
    let client = Arc::new(configured(Offers {
        seen: seen.clone(),
        sibling: Mutex::new(sibling_wait),
        ..Offers::unobserved(io)
    }));
    let control = Arc::clone(&client);
    let id = BoxId::new("offered-budget-unit").unwrap();
    let target = id.clone();
    let worker = std::thread::spawn(move || {
        let asked = Instant::now();
        // A fixture panic is this worker's end too, so the test's wait for it always ends.
        let ended = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            control
                .exec(&target, Program::of(&["cat"]), stdin_alone())
                .map(drop)
        }))
        .unwrap_or_else(|_| Err(Refusal::Broken("the exec's fixture panicked".into())));
        seen.send(Offered::Ended(asked, Instant::now(), ended, armed()))
            .unwrap();
    });
    // The open began, or the exec ended without one: both are facts the worker sends.
    let first = recv_on(&FIRST, "the first event", &events).unwrap();
    let opened = matches!(first, Offered::Opening);
    let reading = client.conn.shared.state().unwrap().reading;
    // The sibling's own deadline is one no answer here comes near: its answer while the open holds is the fact.
    let sibling = ReplyDeadline::caller_before(answer_by(None))
        .and_then(|far| {
            client.conn.call_before(
                wire::MAX_MESSAGE,
                &Request::Call(Call::Start(id.clone())),
                |_, _| Ok(NONE),
                None,
                Some(far),
            )
        })
        .map(|(_, answer)| answer);
    sibling_done.send(()).unwrap();
    let ended = match first {
        Offered::Ended(..) => first,
        Offered::Opening => match recv_on(&ENDED, "the stream's end", &events).unwrap() {
            Offered::Opening => panic!("the exec opened its offer twice"),
            ended => ended,
        },
    };
    worker.join().unwrap();
    let Offered::Ended(asked, returned, ended, log) = ended else {
        panic!("the exec told no end");
    };
    told_path(
        "offered_expiry_releases_only_its_ownership_while_a_sibling_progresses",
        opened,
    );
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
    let open_deadline = *shared.transport.opened.lock().unwrap();
    eprintln!(
        "client_budget_offered cap={cap:?} opened={opened} ended={ended:?} armed={log:?} outbound={outbound:?}"
    );
    let at = armed_instant(&log).expect("the exec armed its deadline in its own thread");
    assert!(asked + cap <= at && at <= returned + cap, "{log:?}");
    let waits: &[&str] = if opened {
        &["starting a client operation", "sending", "the answer"]
    } else {
        &["starting a client operation"]
    };
    assert!(armed_at(&log, waits, Some(at)), "{log:?}");
    assert!(open_deadline.is_none_or(|open| open.at() == at));
    assert!(matches!(&ended, Err(Refusal::Os { why, .. }) if why.contains("caller")));
    assert!(
        returned >= at,
        "refused {:?} before its deadline",
        at.saturating_duration_since(returned)
    );
    assert!(matches!(sibling, Ok(Answer::Done)), "{sibling:?}");
    assert!(clean);
    let starts: Vec<&(u64, Request)> = outbound
        .iter()
        .filter(|(_, request)| matches!(request, Request::Call(Call::Start(_))))
        .collect();
    assert_eq!(starts.len(), 1, "{outbound:?}");
    if opened {
        assert!(!reading, "an offered open must relinquish the reader turn");
        assert_eq!(closed, Some(0));
        assert_eq!(outbound.len(), 3);
        assert_eq!(outbound[0].0, outbound[2].0);
        assert!(matches!(outbound[1].1, Request::Call(Call::Start(_))));
        assert_eq!(outbound[2].1, Request::Release);
    }
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

/// The run (or the delete) the absolute-operation control asks of `client` by `deadline`.
fn absolute_operation<T: Transport>(
    client: &Core<T>,
    run: bool,
    deadline: Instant,
) -> Result<(), Refusal> {
    if run {
        client
            .run_before(
                absolute_spec(),
                Program::of(&["unused"]),
                Stdio::null(),
                deadline,
            )
            .map(drop)
    } else {
        client.delete_before(&BoxId::new("absolute-operation-control").unwrap(), deadline)
    }
}

/// AC-T7.4.1: run/delete use the actual Core answer path with no configured Link cap and an unarmed Measuring wait.
/// Caller expiry is not a learned sample. Every wait each armed, in its own thread, at its one absolute deadline, and its
/// refusal never before it. Each cap is this host's own ([`measured`]: the same call, answered as it is asked), taken as
/// its call starts; whether the host let the call's request go before it is told to the census.
#[test]
fn absolute_operations_cap_unarmed_answers_without_link_defaults() {
    /// The same run or delete, answered.
    static STEPS: Patience = Patience::measuring("test.hex_client.budget.absolute_op_steps");
    /// The run's or the delete's refusal at its absolute cap.
    static CAPPED: Patience = Patience::measuring("test.hex_client.budget.absolute_op_capped");
    for run in [true, false] {
        let reply = if run {
            Answer::Started { pid: 1 }
        } else {
            Answer::Done
        };
        let (cap, answered) = measured(&STEPS, move || {
            absolute_operation(&configured(answering(reply, None)), run, answer_by(None))
        });
        answered.unwrap();
        let (transport, request) = stalled(None);
        let client = Arc::new(configured(AbsoluteStalled(transport)));
        let control = Arc::clone(&client);
        let (finished, answer) = mpsc::sync_channel(1);
        let worker = std::thread::spawn(move || {
            let deadline = Instant::now().checked_add(cap).unwrap();
            let result = absolute_operation(&control, run, deadline);
            let _ = finished.send((deadline, Instant::now(), result, armed()));
        });
        // Its send precedes its answer, so it is read once the answer has come.
        let result = recv_from(&CAPPED, "the capped call's answer", &answer, &worker);
        let observed_send = request.try_recv();
        // Every observation failure still closes this exact connection and joins its actor.
        client.conn.shared.end(Refusal::Broken(
            "absolute operation observation cleanup".into(),
        ));
        let joined = worker.join();
        eprintln!(
            "client_absolute run={run} cap={cap:?} observed_send={observed_send:?} result={result:?} joined={}",
            joined.is_ok()
        );
        joined.unwrap();
        let (deadline, returned, result, log) = result.unwrap();
        let sent = observed_send.is_ok();
        told_path(
            "absolute_operations_cap_unarmed_answers_without_link_defaults",
            sent,
        );
        let waits: &[&str] = if sent {
            &["starting a client operation", "sending", "the answer"]
        } else {
            &["starting a client operation"]
        };
        assert!(armed_at(&log, waits, Some(deadline)), "{log:?}");
        assert!(
            matches!(&result, Err(Refusal::Os { why, .. }) if why.contains("absolute") && !why.contains(LEARNED)),
            "{result:?}"
        );
        assert!(
            returned >= deadline,
            "refused {:?} before its deadline",
            deadline.saturating_duration_since(returned)
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

/// AC-T7.4.1: the same caller deadline bounds sender ownership without taking another sender's guard or positively
/// reporting remote cleanup: every wait the delete armed, in its own thread, at its one absolute deadline, its refusal
/// never before it. The cap is this host's own ([`measured`]: the same delete, answered), taken as the delete starts.
#[test]
fn absolute_delete_cap_covers_queued_sender() {
    /// The same delete, answered.
    static STEPS: Patience = Patience::measuring("test.hex_client.budget.queued_delete_steps");
    /// The queued delete's refusal at its absolute cap, the sender held.
    static CAPPED: Patience = Patience::measuring("test.hex_client.budget.queued_delete_capped");
    let (cap, answered) = measured(&STEPS, || {
        configured(answering(Answer::Done, None)).delete_before(
            &BoxId::new("absolute-operation-control").unwrap(),
            answer_by(None),
        )
    });
    answered.unwrap();
    let (transport, request) = stalled(None);
    let client = Arc::new(configured(AbsoluteStalled(transport)));
    let whole = client.conn.shared.sending.lock(None).unwrap().unwrap();
    let control = Arc::clone(&client);
    let (finished, answer) = mpsc::sync_channel(1);
    let worker = std::thread::spawn(move || {
        let deadline = Instant::now().checked_add(cap).unwrap();
        let result =
            control.delete_before(&BoxId::new("absolute-operation-control").unwrap(), deadline);
        let _ = finished.send((deadline, Instant::now(), result, armed()));
    });
    let result = recv_from(&CAPPED, "the queued delete's answer", &answer, &worker);
    drop(whole);
    if result.is_err() {
        client
            .conn
            .shared
            .end(Refusal::Broken("absolute queued-send cleanup".into()));
    }
    let joined = worker.join();
    joined.unwrap();
    let (deadline, returned, result, log) = result.unwrap();
    let queued = reached(&log, "the send lock");
    told_path("absolute_delete_cap_covers_queued_sender", queued);
    let waits: &[&str] = if queued {
        &["starting a client operation", "sending", "the send lock"]
    } else {
        &["starting a client operation"]
    };
    assert!(armed_at(&log, waits, Some(deadline)), "{log:?}");
    assert!(matches!(&result, Err(Refusal::Os { why, .. }) if why.contains("absolute")));
    assert!(
        returned >= deadline,
        "refused {:?} before its deadline",
        deadline.saturating_duration_since(returned)
    );
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

/// How late a precise wait comes past its deadline on this thread now: poll(2) of no descriptors to a deadline `cap`
/// from now. XNU sleeps it with the kernel's own leeway (an eighth of the wait, at most 1 ms, twice that while the user
/// is idle) whatever the thread's QoS, and it is none of hex-wait's (docs/design/deadline-waits.md), so what it comes
/// late by is the host's own: its timer, and the thread's wait for a core under the load the suites run at.
#[cfg(target_os = "macos")]
fn precise_lateness(cap: Duration) -> Duration {
    let deadline = Instant::now().checked_add(cap).unwrap();
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

/// `refusal`, a refusal at its absolute deadline `cap` from now, then a precise wait as long on the same thread: what
/// the refusal gave, how late past its deadline it came (`None`: before it), and how late the precise wait came, with
/// what the thread armed. The census's evidence of the host's lateness beside the refusal's, held to no bound (the waits
/// audit, ROOT's ruling of 2026-11-13): a loaded host's lateness is its wait for a core, which no bound tells from the
/// wait's own, and the wait's own is held by what it arms (hex-wait's
/// `ac_t7_4_1_a_deadline_wait_arms_a_critical_one_shot_timer_at_its_deadline`).
#[cfg(target_os = "macos")]
fn beside_a_precise_wait<T>(
    cap: Duration,
    refusal: impl FnOnce(Instant) -> T,
) -> (T, Option<Duration>, Duration, Vec<Armed>) {
    let deadline = Instant::now().checked_add(cap).unwrap();
    let refused = refusal(deadline);
    let late = Instant::now().checked_duration_since(deadline);
    (refused, late, precise_lateness(cap), armed())
}

/// The census's rows of how late a deadline test's refusal came, and a precise wait beside it: each told, held to no
/// bound.
#[cfg(target_os = "macos")]
fn tell_lateness(refusal: &Patience, late: Duration, precise: Duration) {
    /// A precise wait's lateness under the suites' load, beside each refusal.
    static PRECISE: Patience = Patience::measuring("test.hex_client.budget.precise_late");
    hex_testkit::waits::answered(refusal, late);
    hex_testkit::waits::answered(&PRECISE, precise);
}

/// AC-T7.4.1: do queue an actual delete behind the held sender, with an absolute deadline; expect it refused as its
/// absolute deadline refuses it, while the sender is still held, so its deadline is the refusal's one cause, and never
/// before that deadline, every wait it armed at that one deadline. How late it came, and a precise wait's lateness beside
/// it, go to the census, held to no bound ([`beside_a_precise_wait`]). The cap is this host's own ([`measured`]: the
/// same delete, answered). macOS times std's condition variable waits by the waiting thread's QoS: at utility QoS a
/// queued sender capped at 25 ms was refused at 95.1 and 98.6 ms (the waits census on 75694786, at cb166de1), which
/// hex-wait's critical timer replaced.
#[cfg(target_os = "macos")]
#[test]
fn ac_t7_4_1_a_queued_sender_is_refused_by_its_deadline() {
    /// The same delete, answered.
    static STEPS: Patience =
        Patience::measuring("test.hex_client.budget.queued_delete_deadline_steps");
    /// The queued delete's refusal at its absolute deadline, the sender held.
    static CAPPED: Patience = Patience::measuring("test.hex_client.budget.queued_delete_deadline");
    /// How late past its deadline it came.
    static LATE: Patience = Patience::measuring("test.hex_client.budget.queued_delete_late");
    let (cap, answered) = measured(&STEPS, || {
        configured(answering(Answer::Done, None))
            .delete_before(&BoxId::new("deadline-control").unwrap(), answer_by(None))
    });
    answered.unwrap();
    let (transport, request) = stalled(None);
    let client = Arc::new(configured(AbsoluteStalled(transport)));
    let whole = client.conn.shared.sending.lock(None).unwrap().unwrap();
    let control = Arc::clone(&client);
    let (finished, answer) = mpsc::sync_channel(1);
    let worker = std::thread::spawn(move || {
        let _ = finished.send(beside_a_precise_wait(cap, |deadline| {
            control.delete_before(&BoxId::new("deadline-control").unwrap(), deadline)
        }));
    });
    let result = recv_from(&CAPPED, "the queued delete's answer", &answer, &worker);
    // Held until its refusal came: no other cause could refuse it.
    drop(whole);
    if result.is_err() {
        client
            .conn
            .shared
            .end(Refusal::Broken("deadline queued-send cleanup".into()));
    }
    worker.join().unwrap();
    let (refused, late, precise, log) = result.unwrap();
    assert!(matches!(refused, Err(Refusal::Os { why, .. }) if why.contains("absolute")));
    let at = armed_instant(&log).expect("the delete armed its deadline in its own thread");
    let queued = reached(&log, "the send lock");
    told_path(
        "ac_t7_4_1_a_queued_sender_is_refused_by_its_deadline",
        queued,
    );
    let waits: &[&str] = if queued {
        &["starting a client operation", "sending", "the send lock"]
    } else {
        &["starting a client operation"]
    };
    assert!(armed_at(&log, waits, Some(at)), "{log:?}");
    tell_lateness(&LATE, late.expect("refused before its deadline"), precise);
    assert!(matches!(request.try_recv(), Err(mpsc::TryRecvError::Empty)));
}

/// AC-T7.4.1: do send an actual delete with an absolute deadline while another call holds the reader's turn, so it
/// waits for its answer on its mailbox; expect it refused as its absolute deadline refuses it, the turn still held, so
/// its deadline is the refusal's one cause, and never before that deadline, every wait it armed at that one deadline.
/// How late it came, and a precise wait's lateness beside it, go to the census, held to no bound
/// ([`beside_a_precise_wait`]). The cap is this host's own ([`measured`]: the same delete, answered). macOS times std's
/// channel receive, a dispatch semaphore's wait, by the waiting thread's QoS, as it times its condition variable.
#[cfg(target_os = "macos")]
#[test]
fn ac_t7_4_1_a_call_waiting_for_its_answer_is_refused_by_its_deadline() {
    /// The same delete, answered.
    static STEPS: Patience = Patience::measuring("test.hex_client.budget.mailbox_deadline_steps");
    /// The delete's refusal at its absolute deadline, its answer waited for on its mailbox.
    static CAPPED: Patience = Patience::measuring("test.hex_client.budget.mailbox_deadline");
    /// How late past its deadline it came.
    static LATE: Patience = Patience::measuring("test.hex_client.budget.mailbox_late");
    let (cap, answered) = measured(&STEPS, || {
        configured(answering(Answer::Done, None))
            .delete_before(&BoxId::new("mailbox-control").unwrap(), answer_by(None))
    });
    answered.unwrap();
    let (transport, request) = stalled(None);
    let client = Arc::new(configured(AbsoluteStalled(transport)));
    // Held, as another call's would be: no call here can take the turn, so the delete waits on its mailbox.
    client.conn.shared.state().unwrap().reading = true;
    let control = Arc::clone(&client);
    let (finished, answer) = mpsc::sync_channel(1);
    let worker = std::thread::spawn(move || {
        let _ = finished.send(beside_a_precise_wait(cap, |deadline| {
            control.delete_before(&BoxId::new("mailbox-control").unwrap(), deadline)
        }));
    });
    let result = recv_from(&CAPPED, "the delete's answer", &answer, &worker);
    if result.is_err() {
        client
            .conn
            .shared
            .end(Refusal::Broken("mailbox deadline cleanup".into()));
    }
    worker.join().unwrap();
    let (refused, late, precise, log) = result.unwrap();
    assert!(matches!(refused, Err(Refusal::Os { why, .. }) if why.contains("absolute")));
    let at = armed_instant(&log).expect("the delete armed its deadline in its own thread");
    let sent = request.try_recv().is_ok();
    told_path(
        "ac_t7_4_1_a_call_waiting_for_its_answer_is_refused_by_its_deadline",
        sent,
    );
    let waits: &[&str] = if sent {
        &["starting a client operation", "sending", "the answer"]
    } else {
        &["starting a client operation"]
    };
    assert!(armed_at(&log, waits, Some(at)), "{log:?}");
    tell_lateness(&LATE, late.expect("refused before its deadline"), precise);
    let state = client.conn.shared.state().unwrap();
    assert!(state.reading && state.calls.is_empty());
}

/// AC-T7.4.1, decision 9: supplied instants survive unchanged; an earlier transport bound may shorten
/// authority, no later/default bound can enlarge it, and a learned wait never replaces it:
/// ending first it is a run-out to record, a tie the caller's; with no caller it bounds nothing,
/// still the run-out to record as it passes (the person's ruling of 2026-11-13). Its spans are the
/// least and the longest one wait can have: none, ending as it is armed, and MOST_ONE_WAIT.
#[test]
fn absolute_caller_limit_keeps_original_instant_and_smaller_bounds() {
    let began = Instant::now();
    let at = began.checked_add(MOST_ONE_WAIT).unwrap();
    let absolute = ReplyDeadline::caller_before(at).unwrap();
    assert_eq!(absolute.at(), at);
    assert_eq!(absolute.limited_by(None).at(), at);
    let earlier = began;
    let configured = ReplyDeadline {
        at: earlier,
        source: ReplyBound::Caller(Duration::ZERO),
    };
    let limited = absolute.limited_by(Some(configured));
    assert_eq!(limited.at(), earlier);
    let later = ReplyDeadline {
        at,
        source: ReplyBound::Caller(MOST_ONE_WAIT),
    };
    assert_eq!(limited.limited_by(Some(later)).at(), earlier);
    let tied = absolute.limited_by(Some(later));
    assert!(matches!(tied.source, ReplyBound::CallerAbsolute));
    let (bound, learned) = ReplyDeadline::combine(Some(absolute), began, None);
    assert_eq!(bound.unwrap().at(), at);
    assert!(learned.is_none());
    let (bound, learned) = ReplyDeadline::combine(Some(absolute), began, Some(Duration::ZERO));
    assert_eq!(bound.unwrap().at(), at);
    let learned = learned.unwrap();
    assert_eq!(learned.at(), earlier);
    assert!(matches!(learned.source, ReplyBound::Learned(_)));
    let (bound, learned) = ReplyDeadline::combine(Some(absolute), began, Some(MOST_ONE_WAIT));
    assert_eq!(bound.unwrap().at(), at);
    assert!(learned.is_none(), "a tie belongs to caller authority");
    // With no caller's deadline nothing bounds the call: its learned wait is the run-out it tells as it passes.
    let (bound, learned) = ReplyDeadline::combine(None, began, Some(Duration::ZERO));
    assert!(bound.is_none(), "{bound:?}");
    let learned = learned.unwrap();
    assert_eq!(learned.at(), earlier);
    assert!(matches!(learned.source, ReplyBound::Learned(_)));
}

/// Shared observations of each actual port and its original optional dial deadline.
type DialDeadlines = Arc<Mutex<Vec<(u32, Option<Instant>)>>>;

struct AbsoluteNamedLink {
    control: Mutex<Option<TcpStream>>,
    /// The stdio streams still to dial, in order: shared, so a test lets go of any its run never dialed.
    stdio: Arc<Mutex<VecDeque<TcpStream>>>,
    deadlines: DialDeadlines,
}

impl Link for AbsoluteNamedLink {
    fn lends_identity(&self) -> bool {
        false
    }

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

/// A loopback connection, connected and accepted. Not a wait for a fact: a connect to a listening socket returns
/// once the kernel has queued it for the accept, which then takes it at once.
fn absolute_pair() -> (TcpStream, TcpStream) {
    let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
    listener.set_nonblocking(true).unwrap();
    let address = listener.local_addr().unwrap();
    let client = TcpStream::connect(address).unwrap();
    let peer = accept_before(
        &listener,
        &AtomicBool::new(false),
        &Wake::new().unwrap(),
        None,
    )
    .unwrap();
    client.set_nodelay(true).unwrap();
    peer.set_nodelay(true).unwrap();
    (client, peer)
}

/// AC-T7.4.1: actual guest Stream run setup with no Link default writes all three native named headers under one caller
/// cap. No server answers the Run, so the same cap must also bound its unarmed control answer and close the asked ends.
/// Every dial and wait the run armed, in its own thread, at its one absolute deadline, its refusal never before it. The
/// cap is this host's own ([`measured`]: the same run, answered); a host too loaded to run the setup's steps within it
/// has the cap end the setup instead: the dials made still share the one cap, the run is refused at it, and the streams
/// it dialed are closed, but the three headers and the request are shown only where the setup came within the cap, the
/// path told to the census. No read here waits on a stream the fixture holds: those the run never dialed are let go
/// before their peers are read (their peers once waited, with no bound, for an end only the fixture's drop would
/// bring: 16 minutes on goal 12's run).
#[test]
fn absolute_run_caps_named_stdio_and_preserves_headers() {
    use hex_core::stream::{self, Named};
    /// The same run, answered.
    static STEPS: Patience = Patience::measuring("test.hex_client.budget.absolute_run_steps");
    /// The run's refusal at its caller's cap, no answer come, the run's thread its party.
    static CAPPED: Patience = Patience::measuring("test.hex_client.budget.absolute_run_capped");
    let (cap, measured_run) = {
        let (control, peer) = control_pair(vec![
            (1, Answer::Started { pid: 7 }),
            (1, Answer::Exited(Exit::Code(0))),
        ]);
        let mut named = VecDeque::new();
        let mut peers = Vec::new();
        for _ in 0..3 {
            let (stream, peer) = absolute_pair();
            named.push_back(stream);
            peers.push(peer);
        }
        let link = guest::own_link(AbsoluteNamedLink {
            control: Mutex::new(Some(control)),
            stdio: Arc::new(Mutex::new(named)),
            deadlines: Arc::default(),
        })
        .unwrap();
        let measured_run = measured(&STEPS, move || -> Result<Exit, Refusal> {
            let client = configured(guest::Stream::dial(link)?);
            let mut proc = client.run_before(
                absolute_spec(),
                Program::of(&["unused"]),
                all_streams(),
                answer_by(None),
            )?;
            hex_api::conformance::end_of(&mut proc)
        });
        drop((peer, peers));
        measured_run
    };
    assert_eq!(measured_run.unwrap(), Exit::Code(0));
    let (control, mut control_peer) = control_pair(Vec::new());
    let mut named = VecDeque::new();
    let mut peers = Vec::new();
    for _ in 0..3 {
        let (stream, peer) = absolute_pair();
        named.push_back(stream);
        peers.push(peer);
    }
    let undialed = Arc::new(Mutex::new(named));
    let deadlines = Arc::new(Mutex::new(Vec::new()));
    let link = guest::own_link(AbsoluteNamedLink {
        control: Mutex::new(Some(control)),
        stdio: Arc::clone(&undialed),
        deadlines: Arc::clone(&deadlines),
    })
    .unwrap();
    let transport = guest::Stream::dial(link).unwrap();
    let client = Arc::new(configured(transport));
    let control = Arc::clone(&client);
    let (finished, answer) = mpsc::sync_channel(1);
    let worker = std::thread::spawn(move || {
        let deadline = Instant::now().checked_add(cap).unwrap();
        let result = control
            .run_before(
                absolute_spec(),
                Program::of(&["unused"]),
                all_streams(),
                deadline,
            )
            .map(drop);
        let _ = finished.send((deadline, Instant::now(), result, armed()));
    });
    let result = recv_from(&CAPPED, "the capped run's answer", &answer, &worker);
    if result.is_err() {
        client
            .conn
            .shared
            .end(Refusal::Broken("absolute named observation cleanup".into()));
    }
    let joined = worker.join();
    let state_clean = {
        let state = client.conn.shared.state().unwrap();
        state.calls.is_empty()
            && state.programs.is_empty()
            && state.given.is_empty()
            && state.closed.is_none()
    };
    let recorded = deadlines.lock().unwrap().clone();
    // The streams the run never dialed, its cap having passed first, let go: every peer then comes to its end, from
    // the run's close or the fixture's.
    let left = {
        let mut undialed = undialed.lock().unwrap();
        let left = undialed.len();
        undialed.clear();
        left
    };
    // Not a wait for a fact: the run has ended, so what it wrote is in the stream and its end on its way.
    let observed: Vec<io::Result<Vec<u8>>> = peers
        .iter_mut()
        .map(|peer| {
            let mut got = Vec::new();
            peer.read_to_end(&mut got).map(|_| got)
        })
        .collect();
    // The client's connection ended, so the control's read ends too: the run's request, if it was sent, then its end.
    client
        .conn
        .shared
        .end(Refusal::Broken("absolute named observation over".into()));
    let request_seen = next_request(&mut control_peer);
    joined.unwrap();
    let (deadline, returned, result, log) = result.unwrap();
    let progressed = left == 0 && request_seen.is_ok();
    told_path(
        "absolute_run_caps_named_stdio_and_preserves_headers",
        progressed,
    );
    eprintln!(
        "client_absolute named_deadlines={recorded:?} caller={deadline:?} result={result:?} left={left} observed={observed:?} request={request_seen:?} armed={log:?}"
    );
    assert!(matches!(
        &result,
        Err(Refusal::Os { op, why }) if why.contains("caller") || *op == "absolute fixture dial"
    ));
    assert!(
        returned >= deadline,
        "refused {:?} before its deadline",
        deadline.saturating_duration_since(returned)
    );
    assert!(
        armed_at(&log, &["starting a client operation"], Some(deadline)),
        "{log:?}"
    );
    assert_eq!(recorded[0], (stream::CONTROL_PORT, None));
    assert!(
        recorded[1..]
            .iter()
            .all(|(port, at)| *port == stream::STDIO_PORT && *at == Some(deadline)),
        "the stdio dials share the run's one cap: {recorded:?}"
    );
    let observed: Vec<Vec<u8>> = observed.into_iter().map(Result::unwrap).collect();
    // Those never dialed came to their end with nothing.
    assert!(observed[3 - left..].iter().all(Vec::is_empty));
    if progressed && let Ok((request_header, request)) = request_seen {
        // The setup came within the cap: the request asked for all three, and each stream carries its header alone.
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
        let names: Vec<Named> = observed
            .iter()
            .map(|got| {
                assert_eq!(
                    got.len(),
                    stream::NAME_BYTES,
                    "an unasked payload or no header"
                );
                let name: &[u8; stream::NAME_BYTES] = got.as_slice().try_into().unwrap();
                stream::read_name(name).unwrap()
            })
            .collect();
        assert!(
            names
                .iter()
                .all(|name| name.exec.conn == TOKEN && name.exec.id == request_header.id)
        );
        assert_eq!(names[0].stream, stream::Stdio::In);
        assert_eq!(names[1].stream, stream::Stdio::Out);
        assert_eq!(names[2].stream, stream::Stdio::Err);
    }
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
/// unfinished RemoteProc. Only the existing sender guard delays that Drop here. Every
/// wait the run armed, and every one the Proc's release armed in the dropping thread,
/// at the run's one absolute deadline. The cap is this host's own ([`measured`]: the
/// same run, answered and its Proc dropped); where the host let the run's answer come
/// only past it, the run's refusal is checked instead, its path told to the census.
#[test]
fn absolute_run_error_return_does_not_renew_drop_sender_budget() {
    /// The same run, answered and its Proc dropped.
    static STEPS: Patience = Patience::measuring("test.hex_client.budget.drop_steps");
    /// The observer's error return, its unfinished Proc dropped with the sender held.
    static RETURNED: Patience = Patience::measuring("test.hex_client.budget.drop_returned");
    let (cap, answered) = measured(&STEPS, || {
        let (io, _) = stalled(None);
        configured(DropOriginal(Offers::unobserved(io)))
            .run_before(
                absolute_spec(),
                Program::of(&["owned-drop-original"]),
                Stdio::null(),
                answer_by(None),
            )
            .map(drop)
    });
    answered.unwrap();
    let (io, _) = stalled(None);
    let client = Arc::new(configured(DropOriginal(Offers::unobserved(io))));
    let _ = armed();
    let original = Instant::now().checked_add(cap).unwrap();
    let ran = client.run_before(
        absolute_spec(),
        Program::of(&["owned-drop-original"]),
        Stdio::null(),
        original,
    );
    let ran_returned = Instant::now();
    let run_log = armed();
    told_path(
        "absolute_run_error_return_does_not_renew_drop_sender_budget",
        ran.is_ok(),
    );
    assert!(
        armed_at(&run_log, &["starting a client operation"], Some(original)),
        "{run_log:?}"
    );
    let mut process = match ran {
        Ok(process) => process,
        Err(refused) => {
            // The host held this thread off a core past its cap before the run's answer: its refusal, never early.
            assert!(
                matches!(&refused, Refusal::Os { why, .. } if why.contains("absolute")),
                "{refused:?}"
            );
            assert!(ran_returned >= original);
            assert!(client.conn.shared.state().unwrap().programs.is_empty());
            return;
        }
    };
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
            let _ = finished.send((Instant::now(), result, armed()));
        })
        .unwrap();

    // From this point every observation is captured; cleanup/join precedes ALL
    // assertions. No production hook, fake clock, new timer or periodic wait.
    let held = client.conn.shared.sending.lock(None);
    let held_exact_guard = matches!(&held, Ok(Some(_)));
    let expired = until(original);
    let released = release.send(());
    // Its notice that it began precedes its answer, so it is read once the answer has come.
    let before_cleanup = recv_on(&RETURNED, "the worker's answer", &answer);
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
        "client_drop_original run_id={run_id} original={original:?} cap={cap:?} held={held_exact_guard} expired={expired:?} gate={released:?} began={began:?} before_release={before_release_at:?} before_cleanup={before_cleanup:?} after_cleanup={after_cleanup:?} release_count={release_count} other_count={other_count} joined={} clean={clean:?}",
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
    let (returned, result, release_log) = before_cleanup.unwrap();
    // The Proc's release, in the dropping thread, armed at the run's original deadline, never a fresh one.
    assert!(
        armed_at(&release_log, &["releasing a program"], Some(original)),
        "{release_log:?}"
    );
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
    /// The caller's budget it gives each call, where it gives one.
    budget: Option<Duration>,
}

impl Transport for BootstrapProbe {
    type Reading = ();

    fn answers() -> &'static [Patience; KINDS] {
        static WAITS: [Patience; KINDS] =
            [const { Patience::measuring("test.client_bootstrap.unarmed") }; KINDS];
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
            budget: None,
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
    fn invoke<T: Transport>(self, client: &Core<T>, at: Instant) -> Result<Answer, Refusal> {
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
                    ended: None,
                    control: None,
                    control_wake: None,
                    credentials: Vec::new(),
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

/// AC-T7.4.1, ROOT's seam of 2026-11-14: do make an actual capacity call under an absolute deadline, answered as it is
/// asked, twice, and once with no deadline, each on this thread, then read this thread's armed log and another's. Expect
/// the capped call's check, send, send lock and answer wait each armed at the one instant given, every other check of it
/// too; the second call's learned wait, its kind answered by then, tagged learned and before that instant; the uncapped
/// call's send and answer wait armed with none; and the other thread's log empty: what a call arms lands in its own
/// thread's. Its calls' learned waits are its own ([`Apart`]): another test's capacity call that ran its wait out had
/// the second call here follow its party, no learned wait armed (r378, the whole crate's tests side by side).
#[test]
fn the_armed_log_holds_each_deadline_an_operation_arms_in_its_own_thread() {
    const CAPPED: &[&str] = &[
        "starting a client operation",
        "sending",
        "the send lock",
        "the answer",
    ];
    let (io, _) = stalled(None);
    let client = configured(Apart::<true>(BootstrapProbe {
        io: Offers::unobserved(io),
        reply: Mutex::new(Some(Answer::Capacity(None))),
        budget: None,
    }));
    let _ = armed();
    let at = answer_by(None);
    assert_eq!(client.capacity_before(Class::Oci, at), Ok(None));
    let first = armed();
    assert!(armed_at(&first, CAPPED, Some(at)), "{first:?}");
    *client.conn.shared.transport.0.reply.lock().unwrap() = Some(Answer::Capacity(None));
    assert_eq!(client.capacity_before(Class::Oci, at), Ok(None));
    let second = armed();
    assert!(armed_at(&second, CAPPED, Some(at)), "{second:?}");
    assert!(
        second.iter().any(|armed| armed.what == "the answer"
            && armed.learned
            && armed.at.is_some_and(|learned| learned < at)),
        "{second:?}"
    );
    *client.conn.shared.transport.0.reply.lock().unwrap() = Some(Answer::Capacity(None));
    assert_eq!(client.capacity(Class::Oci), Ok(None));
    let uncapped = armed();
    assert!(
        armed_at(&uncapped, &["sending", "the answer"], None),
        "{uncapped:?}"
    );
    assert_eq!(std::thread::spawn(armed).join().unwrap(), Vec::new());
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
        // A call that can't be cut: its answer, or its refusal, given no deadline.
        let got = follow::whole(&ANSWERED, "the bootstrap call's answer", || {
            kind.invoke(&client, answer_by(None))
        })
        .unwrap();
        assert_eq!(wire::answer(0, &got, wire::MAX_MESSAGE).unwrap(), expected);
        let outbound = client.conn.shared.transport.io.outbound.lock().unwrap();
        assert_eq!(outbound.len(), 1);
        assert!(kind.matches_request(&outbound[0].1));
        assert!(client.conn.shared.state().unwrap().programs.is_empty());
        drop(outbound);

        let (client, _) = bootstrap_probe(Some(Answer::Done));
        assert!(matches!(
            follow::whole(&ANSWERED, "the bootstrap call's answer", || kind
                .invoke(&client, answer_by(None))),
            Err(Refusal::Broken(_))
        ));

        let why = Refusal::Broken("owned typed bootstrap refusal".into());
        let (client, _) = bootstrap_probe(Some(Answer::Refused(why.clone())));
        assert_eq!(
            follow::whole(&ANSWERED, "the bootstrap call's answer", || kind
                .invoke(&client, answer_by(None))),
            Err(why)
        );
    }
}

/// AC-T7.4.1: even an unarmed answer kind and absent relative transport budget cannot extend the original bootstrap
/// authority. Cleanup/join precedes assertions. Every wait each call armed, in its own thread, at its one absolute
/// deadline, its refusal never before it. Each cap is this host's own ([`measured`]: the same call, answered as it is
/// asked), taken in the call's own thread as it starts; whether the host let its request go before it is told to the
/// census.
#[test]
fn absolute_bootstrap_caps_unarmed_answer_waits() {
    /// The same bootstrap call, answered.
    static STEPS: Patience = Patience::measuring("test.hex_client.budget.bootstrap_steps");
    /// The bootstrap call's refusal at its absolute cap, no answer come.
    static CAPPED: Patience = Patience::measuring("test.hex_client.budget.bootstrap_capped");
    for kind in [
        BootstrapCall::Capacity,
        BootstrapCall::PhysicalReport,
        BootstrapCall::Create,
    ] {
        let (cap, answered) = measured(&STEPS, move || {
            kind.invoke(&configured(answering(kind.reply(), None)), answer_by(None))
        });
        answered.unwrap();
        let (client, sent) = bootstrap_probe(None);
        let client = Arc::new(client);
        let control = Arc::clone(&client);
        let (finished, answer) = mpsc::sync_channel(1);
        let worker = std::thread::spawn(move || {
            let at = Instant::now().checked_add(cap).unwrap();
            let result = kind.invoke(&control, at);
            let _ = finished.send((at, Instant::now(), result, armed()));
        });
        // Its send precedes its answer, so it is read once the answer has come.
        let observed = recv_from(
            &CAPPED,
            "the capped bootstrap call's answer",
            &answer,
            &worker,
        );
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
            "client_bootstrap_absolute kind={kind:?} cap={cap:?} send={observed_send:?} observed={observed:?} joined={} clean={clean:?}",
            joined.is_ok()
        );
        joined.unwrap();
        let (at, returned, refused, log) = observed.unwrap();
        let sent = observed_send.is_ok();
        told_path("absolute_bootstrap_caps_unarmed_answer_waits", sent);
        let waits: &[&str] = if sent {
            &["starting a client operation", "sending", "the answer"]
        } else {
            &["starting a client operation"]
        };
        assert!(armed_at(&log, waits, Some(at)), "{log:?}");
        if sent {
            assert_eq!(outbound.len(), 1);
            assert!(kind.matches_request(&outbound[0].1));
        } else {
            assert!(outbound.is_empty(), "{outbound:?}");
        }
        assert!(clean.unwrap());
        assert!(matches!(
            refused,
            Err(Refusal::Os { why, .. }) if why.contains("absolute") && !why.contains(LEARNED)
        ));
        assert!(
            returned >= at,
            "refused {:?} before its deadline",
            at.saturating_duration_since(returned)
        );
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
/// refusal must not replace run_before's authority with a fresh Kill answer wait: the
/// setup's deadline, and every wait the run and its Kill armed in the run's thread, at
/// the run's one absolute deadline, its refusal never before it. The cap is this host's
/// own ([`measured`]: the same run, its Kill answered), taken in the run's own thread;
/// whether the host let its Kill go before it is told to the census.
#[test]
fn absolute_run_settle_error_does_not_renew_kill_answer_budget() {
    /// The same run, its Kill answered.
    static STEPS: Patience = Patience::measuring("test.hex_client.budget.settle_original_steps");
    /// The run's return, its settle refused and its Kill unanswered within its original authority.
    static RETURNED: Patience =
        Patience::measuring("test.hex_client.budget.settle_original_returned");
    let (cap, answered) = measured(&STEPS, || {
        let (io, _) = stalled(None);
        let (starting, _) = mpsc::sync_channel(1);
        let (killing, _) = mpsc::sync_channel(1);
        configured(SettleOriginal {
            io: Offers::unobserved(io),
            setup: Mutex::new(None),
            started: starting,
            kill: killing,
            exits: AtomicUsize::new(0),
            kill_answer: Mutex::new(Some(Answer::Done)),
            budget: None,
        })
        .run_before(
            absolute_spec(),
            Program::of(&["owned-settle-original"]),
            stdin_alone(),
            answer_by(None),
        )
        .map(drop)
    });
    assert!(answered.is_err(), "the measured run's settle is refused");
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
    let (finished, answer) = mpsc::sync_channel(1);
    let worker = std::thread::Builder::new()
        .name("owned-settle-original".into())
        .spawn(move || {
            let original = Instant::now().checked_add(cap).unwrap();
            let result = control
                .run_before(
                    absolute_spec(),
                    Program::of(&["owned-settle-original"]),
                    stdin_alone(),
                    original,
                )
                .map(drop);
            let _ = finished.send((original, Instant::now(), result, armed()));
        })
        .unwrap();

    // Capture every error branch; end ONLY this connection, close its retained
    // pipe, and join this worker BEFORE asserting any result or phase witness. Its Started
    // and its Kill precede its answer, so each is read once the answer has come.
    let before_cleanup = recv_on(&RETURNED, "the worker's answer", &answer);
    let actual_started = started.try_recv();
    let actual_kill = killed.try_recv();
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
        "client_settle_original cap={cap:?} setup={setup:?} started={actual_started:?} kill={actual_kill:?} runs={run_ids:?} kills={kills:?} exits={exits} releases={releases} before_cleanup={before_cleanup:?} after_cleanup={after_cleanup:?} pipe_owned={pipe_was_owned} pipe_poisoned={pipe_poisoned} joined={} clean={clean:?}",
        joined.is_ok()
    );
    joined.unwrap();
    let (original, returned, result, log) = before_cleanup.unwrap();
    let progressed = !kills.is_empty();
    told_path(
        "absolute_run_settle_error_does_not_renew_kill_answer_budget",
        progressed,
    );
    // What the run armed, its Kill's waits among them: its one deadline, never a fresh one.
    assert!(
        armed_at(&log, &["starting a client operation"], Some(original)),
        "{log:?}"
    );
    assert!(
        setup.is_none_or(|(_, _, deadline)| deadline.at() == original),
        "{setup:?}"
    );
    assert!(!pipe_poisoned);
    assert!(transport.io.io.closed.load(Ordering::Acquire));
    assert!(transport.io.program.lock().unwrap().is_none());
    assert!(clean.unwrap());
    assert_eq!(transport.reply_budget(), None);
    assert_eq!(exits, 0);
    assert!(
        returned >= original,
        "refused {:?} before its deadline",
        original.saturating_duration_since(returned)
    );
    if progressed {
        // Its Started, its settle refused, then one Kill, unanswered: its answer's wait at the run's deadline.
        assert!(pipe_was_owned);
        assert_eq!(run_ids.len(), 1);
        let (exec, ask, _) = setup.unwrap();
        assert_eq!(exec, run_ids[0]);
        assert!(ask.stdin && !ask.stdout && !ask.stderr);
        assert_eq!(actual_started.unwrap().0, exec);
        let (request, target, _) = actual_kill.expect("its Kill went out");
        assert_eq!(target, exec);
        assert_eq!(kills, [(request, exec)]);
        assert!(armed_at(&log, &["the answer"], Some(original)), "{log:?}");
        assert!(
            matches!(&result, Err(Refusal::Lifecycle { id, cause: hex_api::LifecycleCause::Uncertain, .. }) if *id == absolute_spec().id),
            "a Kill unanswered within the original authority leaves the run's end uncertain: {result:?}"
        );
    } else {
        // Its cap passed first: refused by it, before its Kill, never early.
        assert!(
            matches!(&result, Err(Refusal::Os { why, .. }) if why.contains("absolute"))
                || matches!(
                    &result,
                    Err(Refusal::Lifecycle {
                        cause: hex_api::LifecycleCause::Uncertain,
                        ..
                    })
                ),
            "{result:?}"
        );
    }
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
            // A whole scenario, which can't be cut: given no deadline, its own waits following their parties.
            follow::whole(&SETTLED, "a settle failure's scenario", || {
                settle_failure(start, Answer::Refused(refused), None)
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
        assert!(
            matches!(
                &settled.result,
                Err(Refusal::Lifecycle {
                    id,
                    cause: hex_api::LifecycleCause::Uncertain,
                    ..
                }) if *id == settled.id
            ),
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
        .map(|start| {
            follow::whole(&SETTLED, "a settle failure's scenario", || {
                settle_failure(start, Answer::Done, None)
            })
        })
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
/// deadline consumed at setup, no send is attempted and the refusal is the caller's expiry, never
/// before it, every wait the hello armed in its own thread at it. The cap is the hello's learned
/// wait's W* and this host's own time for the hello's steps ([`measured`]: the same hello, its setup
/// lending nothing, answered), so its learned wait ends inside it; it is taken in the hello's own
/// thread, and whether the host let its setup begin before it is told to the census.
#[test]
fn selected_callers_setup_expiry_prevents_send() {
    /// The same hello, answered.
    static STEPS: Patience = Patience::measuring("test.hex_client.budget.selected_setup_steps");
    /// The hello's return, its selected setup authority consumed before any send.
    static RETURNED: Patience =
        Patience::measuring("test.hex_client.budget.selected_setup_returned");
    let hello = Request::Hello {
        build: wire::build(),
    };
    // Its existing private smaller learned wait, the hello's: taught where it has none (its cold start, told).
    let learned = short_learned(&hello, agents_hello());
    let (steps, answered) = measured(&STEPS, || {
        configured(answering(agents_hello(), None))
            .conn
            .call_before(
                wire::MAX_MESSAGE,
                &Request::Hello {
                    build: wire::build(),
                },
                |_, _| Ok(NONE),
                None,
                Some(ReplyDeadline::caller_before(answer_by(None)).unwrap()),
            )
            .map(drop)
    });
    answered.unwrap();
    let cap = learned + steps;
    let (transport, writes) = calibrated::<false>(None, None);
    let client = Arc::new(configured(transport));
    let control = Arc::clone(&client);
    let (selected, selection) = mpsc::sync_channel(1);
    let (finished, answer) = mpsc::sync_channel(1);
    let worker = std::thread::spawn(move || {
        let original = Instant::now().checked_add(cap).unwrap();
        let caller = ReplyDeadline::caller_before(original).unwrap();
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
                        .try_send(deadline)
                        .map_err(|why| Refusal::os("observing selected setup authority", why))?;
                    // Deliberately consume exactly the selected authority, without
                    // replacing it or changing a seed. Caller::send must now emit nothing.
                    until(deadline.at())?;
                    Ok(NONE)
                },
                None,
                Some(caller),
            )
            .map(drop);
        let _ = finished.send((original, Instant::now(), result, armed()));
    });
    // Its selected deadline's notice precedes its answer, so it is read once the answer has come.
    let observed = recv_on(&RETURNED, "the worker's answer", &answer);
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
        "client_hello_phase_a cap={cap:?} learned={learned:?} selected={selected:?} observed={observed:?} writes={writes:?} joined={} clean={clean:?}",
        joined.is_ok()
    );
    joined.unwrap();
    let (original, returned, result, log) = observed.unwrap();
    let progressed = selected.is_ok();
    told_path("selected_callers_setup_expiry_prevents_send", progressed);
    assert!(
        armed_at(&log, &["starting a client operation"], Some(original)),
        "{log:?}"
    );
    assert!(
        returned >= original,
        "refused {:?} before its deadline",
        original.saturating_duration_since(returned)
    );
    // This Stalled transport commits its whole nonempty input on its first try_send;
    // no notification therefore independently witnesses zero send calls/bytes.
    assert!(matches!(writes, Err(mpsc::TryRecvError::Empty)));
    assert!(clean.unwrap());
    if progressed {
        // Its setup lent the caller's deadline, consumed there: its send refused by it, nothing sent.
        let selected = selected.unwrap();
        assert!(matches!(selected.source, ReplyBound::CallerAbsolute));
        assert_eq!(selected.at(), original);
        assert!(armed_at(&log, &["sending"], Some(original)), "{log:?}");
        assert!(
            matches!(&result, Err(Refusal::Os { op: "sending to the node agent", why }) if why.contains("absolute") && !why.contains(LEARNED)),
            "{result:?}"
        );
    } else {
        // Its cap passed first: refused by it as it started, before its setup.
        assert!(
            matches!(&result, Err(Refusal::Os { op: "starting a client operation", why }) if why.contains("absolute") && !why.contains(LEARNED)),
            "{result:?}"
        );
    }
}

/// The ordinary guest Stream, its writes recorded. `MEASURED` names its answers' waits apart: the hello [`measured`]
/// for the cap learns its own, so the capped hello's stay unarmed.
struct UnarmedOrdinaryHello<const MEASURED: bool> {
    stream: guest::Stream,
    writes: Arc<Mutex<Vec<(usize, Instant)>>>,
}

impl<const MEASURED: bool> Transport for UnarmedOrdinaryHello<MEASURED> {
    type Reading = guest::Reading;
    const MAKES_ASKED: bool = <guest::Stream as Transport>::MAKES_ASKED;

    fn answers() -> &'static [Patience; KINDS] {
        static WAITS: [Patience; KINDS] =
            [const { Patience::measuring("test.ordinary_hello_phase.unarmed") }; KINDS];
        static MEASURED_WAITS: [Patience; KINDS] =
            [const { Patience::measuring("test.ordinary_hello_phase.measured") }; KINDS];
        if MEASURED { &MEASURED_WAITS } else { &WAITS }
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

/// The control connection of an ordinary guest Stream whose token, and `answers`, came before its dial, with its peer.
fn ordinary_stream(answers: Vec<(u64, Answer)>) -> (guest::Stream, TcpStream) {
    let (socket, peer) = control_pair(answers);
    let link = guest::own_link(AbsoluteNamedLink {
        control: Mutex::new(Some(socket)),
        stdio: Arc::new(Mutex::new(VecDeque::new())),
        deadlines: Arc::new(Mutex::new(Vec::new())),
    })
    .unwrap();
    // The existing test-only dial consumes a complete prequeued token before the
    // isolated Hello operation. It makes no claim about the public factory's dial phase.
    (guest::Stream::dial(link).unwrap(), peer)
}

/// AC-T7.4.1 control B: the SAME ordinary Stream/Core Hello path under a private
/// unarmed test census sends a full actual frame, gets no answer, and exhausts the
/// caller's original cap, never before it, every wait it armed in its own thread at
/// it. This is NOT the public calibrated constructor fixture. The cap is this host's
/// own ([`measured`]: the same hello, its answer there to read), taken in the hello's
/// own thread; whether the host let its frame go before it is told to the census.
#[test]
fn isolated_ordinary_stream_hello_withheld_answer_uses_original_cap() {
    /// The same hello, answered.
    static STEPS: Patience = Patience::measuring("test.hex_client.budget.ordinary_hello_steps");
    /// The hello's refusal at its caller's cap, no answer come.
    static RETURNED: Patience =
        Patience::measuring("test.hex_client.budget.ordinary_hello_returned");
    let (stream, measured_peer) = ordinary_stream(vec![(1, agents_hello())]);
    let shared = Shared::try_new(UnarmedOrdinaryHello::<true> {
        stream,
        writes: Arc::new(Mutex::new(Vec::new())),
    })
    .unwrap();
    let (cap, answered) = measured(&STEPS, move || {
        let deadline = ReplyDeadline::caller_before(answer_by(None))?;
        Core::hello_before(shared, Some(deadline)).map(drop)
    });
    drop(measured_peer);
    answered.unwrap();
    // The peer sends the token only: no Hello response is queued or synthesized.
    let (stream, mut peer) = ordinary_stream(Vec::new());
    let writes = Arc::new(Mutex::new(Vec::new()));
    let shared = Shared::try_new(UnarmedOrdinaryHello::<false> {
        stream,
        writes: Arc::clone(&writes),
    })
    .unwrap();
    let control = triomphe::Arc::clone(&shared);
    let (finished, answer) = mpsc::sync_channel(1);
    let worker = std::thread::spawn(move || {
        let original = Instant::now().checked_add(cap).unwrap();
        let result = ReplyDeadline::caller_before(original)
            .and_then(|deadline| Core::hello_before(control, Some(deadline)))
            .map(drop);
        let _ = finished.send((original, Instant::now(), result, armed()));
    });
    let observed = recv_from(&RETURNED, "the hello's refusal", &answer, &worker);
    // Read once the hello has ended: a frame is sent only within the hello's own deadline, so what it sent is there,
    // read however late, and a wait for more ends at that deadline, passed.
    let frame = match &observed {
        Ok((original, ..)) => ordinary_hello_frame(
            &mut peer,
            ReplyDeadline {
                at: *original,
                source: ReplyBound::CallerAbsolute,
            },
        ),
        Err(why) => Err(Refusal::Broken(format!("the hello told no end: {why:?}"))),
    };
    let stopped = peer.shutdown(Shutdown::Both);
    drop(peer);
    shared.end(Refusal::Broken("owned ordinary-Hello cleanup".into()));
    let joined = worker.join();
    let committed = writes.lock().unwrap();
    let clean = shared
        .state()
        .map(|state| state.calls.is_empty() && state.programs.is_empty() && state.given.is_empty());
    eprintln!(
        "client_hello_phase_b scope=isolated_ordinary_seam public_constructor_proof=false cap={cap:?} frame={frame:?} writes={committed:?} observed={observed:?} stopped={stopped:?} joined={} clean={clean:?}",
        joined.is_ok()
    );
    joined.unwrap();
    let (original, returned, result, log) = observed.unwrap();
    // Its answer's wait armed: its frame went whole before its cap passed.
    let progressed = reached(&log, "the answer");
    told_path(
        "isolated_ordinary_stream_hello_withheld_answer_uses_original_cap",
        progressed,
    );
    assert!(
        armed_at(&log, &["starting a client operation"], Some(original)),
        "{log:?}"
    );
    assert!(
        returned >= original,
        "refused {:?} before its deadline",
        original.saturating_duration_since(returned)
    );
    assert_eq!(shared.transport.reply_budget(), None);
    assert!(clean.unwrap());
    if progressed {
        // Its whole frame sent, then no answer: its answer's wait at the hello's deadline, refused by it.
        let (header, request, bytes) = frame.expect("its whole frame went before its cap");
        assert_eq!((header.kind, header.id), (1, 1));
        assert!(matches!(request, Request::Hello { build } if build == wire::build()));
        assert_eq!(
            committed.iter().map(|(n, _)| *n).sum::<usize>(),
            bytes.len()
        );
        assert!(
            armed_at(&log, &["sending", "the answer"], Some(original)),
            "{log:?}"
        );
        assert!(
            matches!(&result, Err(Refusal::Os { op: "waiting for the node agent's answer", why }) if why.contains("absolute") && !why.contains(LEARNED)),
            "{result:?}"
        );
    } else {
        // Its cap passed first: refused by it as it started or sent, before any answer's wait.
        assert!(
            matches!(&result, Err(Refusal::Os { op: "starting a client operation" | "sending to the node agent", why }) if why.contains("absolute") && !why.contains(LEARNED)),
            "{result:?}"
        );
    }
}

/// Separate successor transport: the same real memory protocol, with all phase diagnostics
/// deferred until cleanup. The original SettleOriginal and its selector above stay untouched.
struct SettleAligned {
    io: Offers,
    prepared: Mutex<Option<hex_api::Given>>,
    setup: Mutex<Option<(u64, hex_api::Ask, ReplyDeadline)>>,
    started: mpsc::SyncSender<(u64, Instant)>,
    kill: mpsc::SyncSender<(u64, u64, Instant)>,
    /// The agent's answer to the Kill, given as it comes (the measured run's); none withholds it (the capped run's).
    kill_answer: Mutex<Option<Answer>>,
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
            // Record the actual committed Kill: its answer only the one the test gives, never an Exit.
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
            if let Some(answer) = self
                .kill_answer
                .lock()
                .map_err(|why| Sent::Refused(poisoned(why)))?
                .take()
            {
                self.io.queue(header.id, answer);
            }
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

/// A run's Started, as its transport read it: its id and when it came.
type StartedNotice = mpsc::Receiver<(u64, Instant)>;
/// A run's Kill, as its transport took it: the Kill's id, the exec it names, and when it went.
type KillNotice = mpsc::Receiver<(u64, u64, Instant)>;

/// A prepared actor's client ([`SettleAligned`]): its run's Started answered as it is asked, the wrong end (stdout's,
/// where the caller asks for stdin) made before it, and its Kill answered with `kill_answer` or, with none, withheld;
/// with the receivers of its Started's and its Kill's notices.
fn settle_aligned(kill_answer: Option<Answer>) -> (Core<SettleAligned>, StartedNotice, KillNotice) {
    let (io, _) = stalled(None);
    let (starting, started) = mpsc::sync_channel(1);
    let (killing, killed) = mpsc::sync_channel(1);
    // Its Started's and its Kill's answers, and its Run and its Kill: room made before its run, so none is made in it.
    let mut inbound = VecDeque::new();
    inbound.try_reserve(2).unwrap();
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
    let client = configured(SettleAligned {
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
        kill_answer: Mutex::new(kill_answer),
        exits: AtomicUsize::new(0),
        other_answers: AtomicUsize::new(0),
    });
    (client, started, killed)
}

/// AC-T7.4.1 separately pinned successor: start the same cap in an already prepared actor, its cap this host's own
/// ([`measured`]: the same prepared run, its Kill answered), taken in the actor as it is released. Expect the setup's
/// deadline, and every wait the run and its Kill armed in the actor's thread, at the run's one absolute deadline, its
/// refusal never before it; whether the host let its Kill go before it is told to the census.
/// ROOT must compare this SAME selector before/after69a and under the missing-Kill-cap mutation.
#[test]
fn prepared_actor_settle_error_retains_original_kill_answer_authority() {
    /// The same prepared run, its Kill answered.
    static STEPS: Patience = Patience::measuring("test.hex_client.budget.prepared_steps");
    /// The prepared actor's notice that it is ready.
    static READY: Patience = Patience::measuring("test.hex_client.budget.prepared_ready");
    /// The prepared run's return, its settle refused and its Kill unanswered within its original authority.
    static RETURNED: Patience = Patience::measuring("test.hex_client.budget.prepared_returned");
    let (measured_client, _, _) = settle_aligned(Some(Answer::Done));
    let (cap, answered) = measured(&STEPS, move || {
        measured_client
            .run_before(
                absolute_spec(),
                Program::of(&["owned-prepared-settle"]),
                stdin_alone(),
                answer_by(None),
            )
            .map(drop)
    });
    assert!(answered.is_err(), "the measured run's settle is refused");
    let (client, started, killed) = settle_aligned(None);
    let client = Arc::new(client);
    let control = Arc::clone(&client);
    let (prepared, readiness) = mpsc::sync_channel(1);
    let (release, begin) = mpsc::sync_channel(1);
    let (finished, answer) = mpsc::sync_channel(1);
    let spec = absolute_spec();
    let program = Program::of(&["owned-prepared-settle"]);
    let stdio = stdin_alone();
    let worker = std::thread::Builder::new()
        .name("owned-prepared-settle".into())
        .spawn(move || {
            if prepared.try_send(()).is_err() || begin.recv().is_err() {
                return;
            }
            let Some(original) = Instant::now().checked_add(cap) else {
                return;
            };
            // No diagnostics or argument construction separates this one anchor from the actual Core operation.
            let result = control.run_before(spec, program, stdio, original).map(drop);
            let returned = Instant::now();
            let _ = finished.try_send((original, returned, result, armed()));
        })
        .unwrap();

    let actor_ready = recv_on(&READY, "the actor ready", &readiness);
    let released = actor_ready.is_ok().then(|| release.try_send(()));
    // Every early branch closes this owned barrier so joining can never wait for a lost start.
    drop(release);
    // Its Started and its Kill precede its answer, so each is read once the answer has come.
    let before_cleanup = recv_on(&RETURNED, "the worker's answer", &answer);
    let actual_started = started.try_recv();
    let actual_kill = killed.try_recv();
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
    // First diagnostics and all assertions occur only after the owned endpoint/ends close and join.
    eprintln!(
        "client_settle_prepared scope=separate_successor cap={cap:?} ready={actor_ready:?} released={released:?} setup={setup:?} started={actual_started:?} kill={actual_kill:?} runs={runs:?} kills={kills:?} exits={exits} other_answers={other_answers} releases={releases} before_cleanup={before_cleanup:?} after_cleanup={after_cleanup:?} pipe_owned={pipe_was_owned} unused_end={unused_end} pipe_poisoned={pipe_poisoned} end_poisoned={end_poisoned} joined={} clean={clean:?}",
        joined.is_ok()
    );
    joined.unwrap();
    let progressed = !kills.is_empty();
    told_path(
        "prepared_actor_settle_error_retains_original_kill_answer_authority",
        progressed,
    );
    assert!(!pipe_poisoned && !end_poisoned);
    assert!(transport.io.io.closed.load(Ordering::Acquire));
    assert!(transport.io.program.lock().unwrap().is_none());
    assert!(transport.prepared.lock().unwrap().is_none());
    assert!(clean.unwrap());
    assert_eq!(transport.reply_budget(), None);
    assert!(actor_ready.is_ok() && released.is_some_and(|sent| sent.is_ok()));
    assert!(pipe_was_owned);
    assert_eq!(exits, 0);
    assert_eq!(other_answers, 0);
    assert_eq!(releases, 0);
    let (original, returned, result, log) = before_cleanup.unwrap();
    // What the run armed, its Kill's waits among them: its one deadline, never a fresh one.
    assert!(
        armed_at(&log, &["starting a client operation"], Some(original)),
        "{log:?}"
    );
    assert!(
        setup.is_none_or(|(_, _, deadline)| deadline.at() == original),
        "{setup:?}"
    );
    assert!(
        returned >= original,
        "refused {:?} before its deadline",
        original.saturating_duration_since(returned)
    );
    if progressed {
        // Its Started, its settle refused, then one Kill, unanswered: its answer's wait at the run's deadline.
        assert!(!unused_end);
        assert_eq!(runs.len(), 1);
        let (exec, ask, _) = setup.unwrap();
        assert_eq!(exec, runs[0]);
        assert!(ask.stdin && !ask.stdout && !ask.stderr);
        assert_eq!(actual_started.unwrap().0, exec);
        let (request, target, _) = actual_kill.unwrap();
        assert_eq!(target, exec);
        assert_eq!(kills, [(request, exec)]);
        assert!(armed_at(&log, &["the answer"], Some(original)), "{log:?}");
        assert!(
            matches!(&result, Err(Refusal::Lifecycle { id, cause: hex_api::LifecycleCause::Uncertain, .. }) if *id == absolute_spec().id),
            "a Kill unanswered within the original authority leaves the run's end uncertain: {result:?}"
        );
    } else {
        // Its cap passed first: refused by it, before its Kill, never early.
        assert!(
            matches!(&result, Err(Refusal::Os { why, .. }) if why.contains("absolute"))
                || matches!(
                    &result,
                    Err(Refusal::Lifecycle {
                        cause: hex_api::LifecycleCause::Uncertain,
                        ..
                    })
                ),
            "{result:?}"
        );
    }
}

/// A call that gave up waiting: its late answers have no taker.
const GAVE_UP: u64 = u64::MAX;

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
    /// Whether the turn under way has read: a turn reads once, the claim these tests make.
    read_in_turn: AtomicBool,
    /// The fuse: a turn's second read while answers for others keep coming, a poll that keeps reading, ends the
    /// connection, the only way such a poll comes back. Its basis is that claim, never a count.
    blown: AtomicBool,
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
            read_in_turn: AtomicBool::new(false),
            blown: AtomicBool::new(false),
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
        self.read_in_turn.store(false, Ordering::SeqCst);
        self.socket.handle()
    }
    fn recv(&self, _: &mut (), take: &mut Take<'_>) -> Result<bool, Refusal> {
        self.socket.drain();
        if self.read_in_turn.swap(true, Ordering::SeqCst) && self.chatting.load(Ordering::SeqCst) {
            self.blown.store(true, Ordering::SeqCst);
            return Err(Refusal::Broken(
                "the fuse: a turn read again while answers for others kept coming".into(),
            ));
        }
        let next = self.next.lock().unwrap().pop_front();
        let message = match next {
            Some(message) => message,
            None if self.chatting.load(Ordering::SeqCst) => {
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
        "client_poll_bound phase=try_wait polled={polled:?} zero={zero:?} ended={ended:?} again={again:?} others={others} fuse_blown={} handed_on={handed_on}",
        chatter.blown.load(Ordering::SeqCst)
    );
    assert_eq!(
        polled,
        (Ok(None), 1, 1),
        "a poll must take one turn and read once while answers for others keep coming"
    );
    assert_eq!(zero, (Ok(None), 1, 1));
    assert_eq!(ended, (Ok(Some(Exit::Code(7))), 1, 0));
    assert_eq!(again, (Ok(Some(Exit::Code(7))), 0, 0));
    assert!(
        !chatter.blown.load(Ordering::SeqCst),
        "a turn read on: the fuse ended the connection"
    );
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
        "client_poll_bound phase=kill killed={killed:?} kills={kills:?} exec={exec} others={others} fuse_blown={}",
        chatter.blown.load(Ordering::SeqCst)
    );
    assert_eq!(
        killed,
        (Ok(()), 2, 1),
        "kill's check for an end must take one turn and read once, then one turn for its answer"
    );
    assert_eq!(kills, [exec]);
    assert!(
        !chatter.blown.load(Ordering::SeqCst),
        "a turn read on: the fuse ended the connection"
    );
}

/// AC-T7.4.1: do `wait_for` the least positive wait, a nanosecond, Duration's own unit, on a remote
/// program while answers for others keep coming; expect `Ok(None)` once its time is spent, each turn
/// reading once, never reading on until the fuse ends the connection. Then with its end next: expect a
/// spent wait to take that end from its read.
#[test]
fn a_timed_wait_ends_at_its_deadline_while_answers_for_others_keep_coming() {
    let client = configured(Chatter::new());
    let (mut proc, exec) = chatting_program(&client);
    let chatter = &client.conn.shared.transport;
    chatter.chat();
    let wait = Duration::from_nanos(1);
    let waited = chatter.counted(|| proc.wait_for(wait));
    chatter.queue(exec, &Answer::Exited(Exit::Code(3)));
    let ended = proc.wait_for(wait);
    eprintln!(
        "client_wait_bound phase=wait_for waited={waited:?} ended={ended:?} fuse_blown={}",
        chatter.blown.load(Ordering::SeqCst)
    );
    assert_eq!(
        waited.0,
        Ok(None),
        "a timed wait came back with its time spent"
    );
    assert!(
        !chatter.blown.load(Ordering::SeqCst),
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
    // A deadline the test never reaches, the longest one wait ([`answer_by`]).
    let far = answer_by(None);
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
        "client_wait_bound phase=setup_kill ran={ran:?} run={run:?} kills={kills:?} fuse_blown={}",
        chatter.blown.load(Ordering::SeqCst)
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
    // A poll with no deadline, which ends at its party's end, the pipe's last reader gone (its HUP or ERR), or at its
    // room.
    let polled = follow::whole(
        &READERLESS,
        "the late end's reader gone or its room",
        || rustix::event::poll(&mut room, None),
    );
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
/// the run's caller expiry, never before its deadline, every wait it armed at it; the list answered,
/// nothing kept for the run, the late end closed (its pipe has no reader left), and the Taken word
/// for it sent, so the agent lets its copy go. The cap is this host's own ([`measured`]: the same
/// run, its end given as it is asked), taken in the run's own thread; whether the host let its
/// request go before it is told to the census.
#[test]
fn a_given_answer_after_its_caller_gave_up_closes_its_ends() {
    /// The same run, its stdout's end given as it is asked.
    static STEPS: Patience = Patience::measuring("test.hex_client.budget.late_given_steps");
    let late_agent = || {
        configured(Late {
            wake: Wake::new().unwrap(),
            next: Mutex::new(VecDeque::new()),
            sent: Mutex::new(Vec::new()),
        })
    };
    let stdout_alone = || Stdio {
        ask: hex_api::Ask {
            stdout: true,
            ..hex_api::Ask::default()
        },
        ..Stdio::null()
    };
    let given = Answer::Given {
        pid: 9,
        given: Streams {
            stdout: true,
            ..Streams::default()
        },
    };
    let measured_client = late_agent();
    let (measured_output, measured_program) = std::io::pipe().unwrap();
    // Its first call's id, the run's: its end there to read as it is asked.
    measured_client
        .conn
        .shared
        .transport
        .queue(1, &given, vec![measured_output.into()]);
    let measured_stdio = stdout_alone();
    let (cap, answered) = measured(&STEPS, move || {
        measured_client
            .run_before(
                absolute_spec(),
                Program::of(&["late-given"]),
                measured_stdio,
                answer_by(None),
            )
            .map(drop)
    });
    drop(measured_program);
    answered.unwrap();
    let client = late_agent();
    let late = &client.conn.shared.transport;
    // This thread's log, from here on the run's alone.
    armed();
    let original = Instant::now().checked_add(cap).unwrap();
    let gave_up = client
        .run_before(
            absolute_spec(),
            Program::of(&["late-given"]),
            stdout_alone(),
            original,
        )
        .map(drop);
    let returned = Instant::now();
    let log = armed();
    let sent_run = late.sent.lock().unwrap().first().map(|(id, _)| *id);
    told_path(
        "a_given_answer_after_its_caller_gave_up_closes_its_ends",
        sent_run.is_some(),
    );
    eprintln!("client_late_given cap={cap:?} gave_up={gave_up:?} run={sent_run:?} armed={log:?}");
    assert!(
        armed_at(&log, &["starting a client operation"], Some(original)),
        "{log:?}"
    );
    assert!(
        returned >= original,
        "refused {:?} before its deadline",
        original.saturating_duration_since(returned)
    );
    assert!(
        matches!(&gave_up, Err(Refusal::Os { why, .. }) if why.contains("absolute")),
        "{gave_up:?}"
    );
    let Some(run) = sent_run else {
        // Its cap passed before its request went: nothing of it for a late answer to name.
        assert_eq!(client.list(), Ok(Vec::new()));
        assert!(client.conn.shared.state().unwrap().given.is_empty());
        return;
    };
    assert!(armed_at(&log, &["the answer"], Some(original)), "{log:?}");
    let (output, program) = std::io::pipe().unwrap();
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
    eprintln!("client_late_given listed={listed:?} kept={kept} closed={closed} taken={taken}");
    assert_eq!(listed, Ok(Vec::new()));
    assert!(
        !kept,
        "the late answer's ends were kept for a run no one waits on"
    );
    assert!(closed, "the late stdout end is still open");
    assert!(taken, "no Taken word for the late answer");
}
