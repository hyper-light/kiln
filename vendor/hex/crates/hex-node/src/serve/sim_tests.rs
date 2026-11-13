//! The socket's core over a simulated OS (native plan step 8: one node core for every OS, over each OS's
//! event sources): the same requests, bounds and disconnect rule the Linux tests drive through a real
//! socket (linux_tests.rs, HEX-SOCK-3 to 10), here through an [`Os`] whose connections are queues and whose
//! programs end when the test says, so they run on every OS the core builds for. The simulated OS is
//! level-triggered, as epoll is by default: a watched connection is readable while messages wait or its
//! client has gone, and writable while it has room.

use super::*;

use std::collections::BTreeSet;
use std::sync::MutexGuard;
use std::sync::atomic::AtomicUsize;

use hex_api::sim::SimRuntime;
use hex_api::{Bounds, BoxInfo, BoxSpec, ClassSpec, Signal, Usage};

/// Longest a test waits for a fact it needs: a bound, so a core that never makes it true fails the test
/// instead of hanging it.
const PATIENCE: Duration = Duration::from_secs(30);

/// One simulated connection: what the client sent and what the server sent back.
#[derive(Default)]
struct Sock {
    to_server: Mutex<VecDeque<Vec<u8>>>,
    to_client: Mutex<VecDeque<Vec<u8>>>,
    /// Messages the server may send before it would block.
    room: Mutex<usize>,
    client_gone: AtomicBool,
    shut: AtomicBool,
    /// Signalled when the server sends.
    sent: Condvar,
    sent_lock: Mutex<()>,
}

impl Sock {
    fn new(room: usize) -> Arc<Self> {
        Arc::new(Self {
            room: Mutex::new(room),
            ..Self::default()
        })
    }
}

#[derive(Default)]
struct SimState {
    pending: VecDeque<(Arc<Sock>, bool)>,
    accepting: bool,
    spent: bool,
    woken: bool,
    conns: BTreeMap<u64, (Arc<Sock>, Interest)>,
    /// Programs watched, by pid: their token, and whether they have ended.
    programs: BTreeMap<u32, (u64, bool)>,
    /// Programs killed by their own runtime ([`Proc::kill`]).
    proc_killed: BTreeSet<u32>,
    /// Every pid the core asked to watch as a host process.
    ever_watched: BTreeSet<u32>,
    unwatchable: bool,
    /// Programs reaped by their runtime before the core watched them (a run's, whose node reaps it to remove its box):
    /// a watch of one finds it gone.
    reaped: BTreeSet<u32>,
    /// Programs ended whose runtime hasn't told their end yet (a run's, its box being removed).
    untold: BTreeSet<u32>,
    /// How many times each program's watch was reported ready.
    reported: BTreeMap<u32, usize>,
}

#[derive(Default)]
struct WaitHooks {
    next_lock: Option<Box<dyn FnOnce() + Send>>,
    before_sleep: Option<Box<dyn FnOnce() + Send>>,
    after_proc_check: Option<Box<dyn FnOnce() + Send>>,
}

#[derive(Default)]
struct SimOs {
    state: Mutex<SimState>,
    changed: Condvar,
    hooks: Mutex<WaitHooks>,
}

impl SimOs {
    fn lock(&self) -> MutexGuard<'_, SimState> {
        let hook = self.hooks.lock().unwrap().next_lock.take();
        if let Some(hook) = hook {
            hook();
        }
        self.state.lock().unwrap()
    }

    fn before_sleep(&self) {
        let hook = self.hooks.lock().unwrap().before_sleep.take();
        if let Some(hook) = hook {
            hook();
        }
    }

    fn after_proc_check(&self) {
        let hook = self.hooks.lock().unwrap().after_proc_check.take();
        if let Some(hook) = hook {
            hook();
        }
    }

    fn poke(&self) {
        // Socket predicates live behind their own locks/atomics. Join the waiter's state mutex
        // before notifying, so a producer cannot signal after its scan but before it sleeps.
        let _state = self.lock();
        self.changed.notify_all();
    }

    /// A client connects; `allowed` says whether it is this user.
    fn connect(&self, allowed: bool, room: usize) -> Arc<Sock> {
        let sock = Sock::new(room);
        self.lock().pending.push_back((Arc::clone(&sock), allowed));
        self.poke();
        sock
    }

    fn put(&self, sock: &Sock, bytes: Vec<u8>) {
        sock.to_server.lock().unwrap().push_back(bytes);
        self.poke();
    }

    fn leave(&self, sock: &Sock) {
        sock.client_gone.store(true, Ordering::SeqCst);
        self.poke();
    }

    fn give_room(&self, sock: &Sock, room: usize) {
        *sock.room.lock().unwrap() = room;
        self.poke();
    }

    /// The program `pid` ends of itself.
    fn end(&self, pid: u32) {
        if let Some(p) = self.lock().programs.get_mut(&pid) {
            p.1 = true;
        }
        self.poke();
    }

    /// The program `pid` is killed by its own runtime, as its `Proc::kill` does.
    fn proc_kill(&self, pid: u32) {
        let mut s = self.lock();
        s.proc_killed.insert(pid);
        if let Some(p) = s.programs.get_mut(&pid) {
            p.1 = true;
        }
        drop(s);
        self.poke();
    }

    fn ended(&self, pid: u32) -> bool {
        self.lock().programs.get(&pid).is_none_or(|p| p.1)
    }

    /// The runtime of program `pid` tells its end now.
    fn tell(&self, pid: u32) {
        self.lock().untold.remove(&pid);
        self.poke();
    }
}

struct Accept(Arc<Sock>, bool);

impl Os for SimOs {
    type Conn = Accept;
    type Watch = u32;
    type Passed = ();

    fn wait(&self, events: &mut Vec<Event>, timeout: Option<Duration>) -> Result<(), Refusal> {
        let deadline = timeout.and_then(|t| Instant::now().checked_add(t));
        let mut s = self.lock();
        loop {
            if std::mem::take(&mut s.woken) {
                events.push(Event::Wake);
            }
            if s.accepting && !s.pending.is_empty() {
                events.push(Event::Listener);
            }
            for (token, (sock, interest)) in &s.conns {
                let gone = sock.client_gone.load(Ordering::SeqCst);
                let readable = matches!(interest, Interest::Read)
                    && (gone || !sock.to_server.lock().unwrap().is_empty());
                let writable =
                    matches!(interest, Interest::Write) && *sock.room.lock().unwrap() > 0;
                if readable || writable || gone {
                    events.push(Event::Ready {
                        token: *token,
                        readable,
                        writable,
                        hung_up: gone,
                    });
                }
            }
            let ready: Vec<(u32, u64)> = s
                .programs
                .iter()
                .filter(|(_, (_, ended))| *ended)
                .map(|(pid, (token, _))| (*pid, *token))
                .collect();
            for (pid, token) in ready {
                *s.reported.entry(pid).or_default() += 1;
                events.push(Event::Ready {
                    token,
                    readable: true,
                    writable: false,
                    hung_up: false,
                });
            }
            if !events.is_empty() {
                return Ok(());
            }
            self.before_sleep();
            match deadline {
                None => s = self.changed.wait(s).unwrap(),
                Some(d) => {
                    let left = d.saturating_duration_since(Instant::now());
                    if left.is_zero() {
                        return Ok(());
                    }
                    s = self.changed.wait_timeout(s, left).unwrap().0;
                }
            }
        }
    }

    fn wake(&self) -> Result<(), Refusal> {
        self.lock().woken = true;
        self.poke();
        Ok(())
    }

    fn accept(&self) -> Accepted<Accept> {
        let mut s = self.lock();
        if s.spent {
            return Accepted::Spent;
        }
        match s.pending.pop_front() {
            Some((sock, allowed)) => Accepted::Conn(Accept(sock, allowed)),
            None => Accepted::None,
        }
    }

    fn accepting(&self, on: bool) -> Result<(), Refusal> {
        self.lock().accepting = on;
        Ok(())
    }

    fn peer(&self, conn: &Accept) -> Peer {
        if conn.1 {
            Peer::Allowed
        } else {
            Peer::Refused("a simulated stranger".to_owned())
        }
    }

    fn watch(&self, conn: &Accept, token: u64, interest: Interest) -> Result<(), Refusal> {
        self.lock()
            .conns
            .insert(token, (Arc::clone(&conn.0), interest));
        self.poke();
        Ok(())
    }

    fn rewatch(&self, conn: &Accept, token: u64, interest: Interest) -> Result<(), Refusal> {
        self.watch(conn, token, interest)
    }

    fn unwatch(&self, conn: &Accept) {
        self.lock()
            .conns
            .retain(|_, (s, _)| !Arc::ptr_eq(s, &conn.0));
    }

    fn recv(&self, conn: &Accept, buf: &mut [u8]) -> Recv<()> {
        match conn.0.to_server.lock().unwrap().pop_front() {
            Some(bytes) => {
                let n = bytes.len().min(buf.len());
                buf[..n].copy_from_slice(&bytes[..n]);
                Recv::Message {
                    size: bytes.len(),
                    got: n,
                    passed: Vec::new(),
                    cut: false,
                }
            }
            None if conn.0.client_gone.load(Ordering::SeqCst) => Recv::Message {
                size: 0,
                got: 0,
                passed: Vec::new(),
                cut: false,
            },
            None => Recv::WouldBlock,
        }
    }

    fn send(&self, conn: &Accept, bytes: &[u8], _fds: &[hex_api::End]) -> Sent {
        let sock = &conn.0;
        if sock.client_gone.load(Ordering::SeqCst) || sock.shut.load(Ordering::SeqCst) {
            return Sent::Gone;
        }
        {
            let mut room = sock.room.lock().unwrap();
            match room.checked_sub(1) {
                Some(left) => *room = left,
                None => return Sent::WouldBlock,
            }
        }
        sock.to_client.lock().unwrap().push_back(bytes.to_vec());
        let _held = sock.sent_lock.lock().unwrap();
        sock.sent.notify_all();
        Sent::Sent
    }

    fn shutdown(&self, conn: &Accept) {
        conn.0.shut.store(true, Ordering::SeqCst);
        let _held = conn.0.sent_lock.lock().unwrap();
        conn.0.sent.notify_all();
    }

    fn hung_up(&self, conn: &Accept) -> bool {
        conn.0.client_gone.load(Ordering::SeqCst)
    }

    fn watch_program(&self, pid: u32, token: u64) -> Result<u32, Unwatched> {
        let mut s = self.lock();
        if s.reaped.contains(&pid) {
            return Err(Unwatched::Gone);
        }
        if s.unwatchable {
            return Err(Unwatched::Killed(Refusal::Os {
                op: "watching a started program",
                why: "a simulated failure: the node agent ended it".to_owned(),
            }));
        }
        s.programs.insert(pid, (token, false));
        s.ever_watched.insert(pid);
        Ok(pid)
    }

    fn unwatch_program(&self, watch: u32) {
        self.lock().programs.remove(&watch);
    }

    fn stdio(&self, _streams: Streams, _passed: Vec<()>) -> Result<Stdio, Refusal> {
        Ok(Stdio::null())
    }

    fn stop_listening(&self) {
        self.lock().accepting = false;
    }

    fn watch_source(&self, _: super::Handle<'_>, _: u64, _: bool) -> Result<(), Refusal> {
        Err(Refusal::Broken(
            "the simulated OS watches no host connection".to_owned(),
        ))
    }

    fn unwatch_source(&self, _: super::Handle<'_>, _: u64) {}
}

/// A program that ends when the simulated OS says it has, or `Signal(9)` once its own runtime kills it
/// ([`Proc::kill`]).
struct SimProc {
    pid: u32,
    os: Arc<SimOs>,
}

impl SimProc {
    fn exit_in(&self, s: &SimState) -> Option<Exit> {
        if s.untold.contains(&self.pid) {
            None
        } else if s.proc_killed.contains(&self.pid) {
            Some(Exit::Signal(9))
        } else if s.programs.get(&self.pid).is_none_or(|p| p.1) {
            Some(Exit::Code(0))
        } else {
            None
        }
    }
}

impl Proc for SimProc {
    fn pid(&self) -> u32 {
        self.pid
    }
    fn try_wait(&mut self) -> Result<Option<Exit>, Refusal> {
        let s = self.os.lock();
        Ok(self.exit_in(&s))
    }
    /// Waits until its end can be told, at most `timeout`.
    fn wait_for(&mut self, timeout: Duration) -> Result<Option<Exit>, Refusal> {
        let deadline = Instant::now().checked_add(timeout);
        let mut s = self.os.lock();
        loop {
            if let Some(exit) = self.exit_in(&s) {
                return Ok(Some(exit));
            }
            self.os.after_proc_check();
            let left = deadline.map_or(timeout, |d| d.saturating_duration_since(Instant::now()));
            if left.is_zero() {
                return Ok(None);
            }
            self.os.before_sleep();
            // The checked predicate and condvar registration share one guard; an end cannot
            // arrive between a discarded predicate guard and a fresh sleep guard.
            s = self.os.changed.wait_timeout(s, left).unwrap().0;
        }
    }
    fn kill(&mut self) -> Result<(), Refusal> {
        self.os.proc_kill(self.pid);
        Ok(())
    }
    fn streams(&mut self) -> Result<hex_api::Given, Refusal> {
        Ok(hex_api::Given::default())
    }
}

/// A simulated node in a guest: box [`g`]'s calls are forwarded to it ([`Host::forward`]) and answered when
/// its gate is open (held, a guest slow to answer), and its programs' pids are the guest's, which no host
/// process has; their ends come from it, as messages.
/// An answer the simulated guest gives once its gate is open.
type Waiting = Box<dyn FnOnce() + Send>;

#[derive(Default)]
struct Guest {
    /// Whether its gate is shut, and the answers waiting behind it, oldest first.
    gate: Mutex<(bool, Vec<Waiting>)>,
    /// What takes each running program's end, by guest pid.
    programs: Mutex<BTreeMap<u32, Ended>>,
}

impl Guest {
    fn hold(&self, held: bool) {
        let waiting = {
            let mut gate = self.gate.lock().unwrap();
            gate.0 = held;
            if held {
                Vec::new()
            } else {
                std::mem::take(&mut gate.1)
            }
        };
        for answer in waiting {
            answer();
        }
    }

    fn holding(&self) -> usize {
        self.gate.lock().unwrap().1.len()
    }

    /// Gives `answer` now, or once the gate opens.
    fn answer(&self, answer: Waiting) {
        let mut gate = self.gate.lock().unwrap();
        if gate.0 {
            gate.1.push(answer);
        } else {
            drop(gate);
            answer();
        }
    }

    /// Its program `pid` ends with `exit`, unless it has ended or been let go.
    fn end(&self, pid: u32, exit: Exit) {
        let taker = self.programs.lock().unwrap().remove(&pid);
        if let Some(ended) = taker {
            ended(Ok(exit));
        }
    }
}

/// A program in the guest, by its guest pid.
struct GuestProgram {
    pid: u32,
    guest: Arc<Guest>,
}

impl Remote for GuestProgram {
    fn kill(&self, done: Killed) {
        let (pid, guest) = (self.pid, Arc::clone(&self.guest));
        // Answered later, as a guest's answer comes on its connection.
        std::thread::spawn(move || {
            done(Ok(()));
            guest.end(pid, Exit::Signal(9));
        });
    }
    fn release(&self) {
        self.guest.programs.lock().unwrap().remove(&self.pid);
    }
}

/// HEX-RT-17: the guest can fail before its host returns the offer, after the wire offer, or inside
/// Ready's send. The deferred closure owns a cancellation guard, just as a guest control connection
/// owns the unsent exec; the real server decides when to run or drop it.
#[derive(Clone, Copy)]
enum OfferFailure {
    BeforeRegistration,
    AfterOffered,
    DuringReadySend,
    ReadySendFailureFirst,
    RefusedBeforeGuestError,
}

struct OfferedGuest {
    failure: OfferFailure,
    reply: Mutex<Option<Reply>>,
    cancellations: Mutex<Vec<()>>,
    ready_calls: Mutex<Vec<()>>,
}

impl OfferedGuest {
    fn new(failure: OfferFailure) -> Arc<Self> {
        Arc::new(Self {
            failure,
            reply: Mutex::new(None),
            cancellations: Mutex::new(Vec::new()),
            ready_calls: Mutex::new(Vec::new()),
        })
    }

    fn refusal() -> Refusal {
        Refusal::os("simulated guest reply", "the guest connection failed")
    }

    fn host_refusal() -> Refusal {
        Refusal::os("simulated offer identity", "the guest changed")
    }

    fn fail(&self) {
        let reply = self.reply.lock().unwrap().take().unwrap();
        reply(Err(Self::refusal()));
    }

    fn offer(self: &Arc<Self>, pid: u32, reply: Reply) -> Forward {
        assert!(self.reply.lock().unwrap().replace(reply).is_none());
        let cancelled = CancelOffered(Arc::clone(self));
        let guest = Arc::clone(self);
        let offering = Offering {
            streams: wire::StreamOffer {
                pid,
                started: u64::from(pid),
                vsock: PathBuf::from("/simulated/guest-vsock"),
                token: u64::from(pid),
                exec: u64::from(pid),
                streams: Streams::default(),
            },
            ready: Box::new(move || {
                let _cancelled = cancelled;
                guest.ready_calls.lock().unwrap().push(());
                if matches!(guest.failure, OfferFailure::DuringReadySend) {
                    // The guest's terminal answer wins inside a send that itself reports failure.
                    guest.fail();
                    Err(Refusal::os(
                        "simulated Ready send",
                        "the control connection closed",
                    ))
                } else if matches!(guest.failure, OfferFailure::ReadySendFailureFirst) {
                    Err(Refusal::os(
                        "simulated Ready send",
                        "the control connection closed",
                    ))
                } else {
                    Ok(())
                }
            }),
        };
        if matches!(self.failure, OfferFailure::BeforeRegistration) {
            self.fail();
        }
        if matches!(self.failure, OfferFailure::RefusedBeforeGuestError) {
            // A deferred callback exists, but checking the offer's guest identity failed locally.
            drop(offering);
            return Forward::Refused(Self::host_refusal());
        }
        Forward::Offered(offering)
    }
}

struct CancelOffered(Arc<OfferedGuest>);

impl Drop for CancelOffered {
    fn drop(&mut self) {
        self.0.cancellations.lock().unwrap().push(());
    }
}

/// Boxes in the simulator; each exec a [`SimProc`]; inspect held shut on purpose to keep calls in flight.
struct SimHost {
    sim: SimRuntime,
    os: Arc<SimOs>,
    next_pid: AtomicUsize,
    shut: Mutex<bool>,
    opened: Condvar,
    abandoned: Mutex<Vec<BoxId>>,
    guest: Arc<Guest>,
    offered: Mutex<Option<Arc<OfferedGuest>>>,
}

impl SimHost {
    fn new(os: &Arc<SimOs>) -> Arc<Self> {
        let host = Arc::new(Self {
            sim: SimRuntime::new(),
            os: Arc::clone(os),
            next_pid: AtomicUsize::new(100),
            shut: Mutex::new(false),
            opened: Condvar::new(),
            abandoned: Mutex::new(Vec::new()),
            guest: Arc::default(),
            offered: Mutex::new(None),
        });
        for id in [b(), g()] {
            host.sim
                .create(BoxSpec {
                    route: None,
                    allocation: None,
                    devices: Vec::new(),
                    id,
                    bounds: Bounds::default(),
                    class: ClassSpec::Process,
                    owner: None,
                    band: 0,
                })
                .unwrap();
        }
        host
    }

    fn hold(&self, shut: bool) {
        *self.shut.lock().unwrap() = shut;
        self.opened.notify_all();
    }
}

impl Runtime for SimHost {
    fn classes(&self) -> Vec<hex_api::Class> {
        self.sim.classes()
    }
    fn create(&self, spec: BoxSpec) -> Result<BoxInfo, Refusal> {
        self.sim.create(spec)
    }
    fn start(&self, id: &BoxId) -> Result<(), Refusal> {
        self.sim.start(id)
    }
    fn exec(&self, id: &BoxId, _program: Program, _stdio: Stdio) -> Result<Box<dyn Proc>, Refusal> {
        self.sim.inspect(id)?;
        let pid = u32::try_from(self.next_pid.fetch_add(1, Ordering::SeqCst)).unwrap();
        Ok(Box::new(SimProc {
            pid,
            os: Arc::clone(&self.os),
        }))
    }
    /// The run's box in the simulator, its program a [`SimProc`] as an exec's is.
    fn run(&self, spec: BoxSpec, program: Program, stdio: Stdio) -> Result<Box<dyn Proc>, Refusal> {
        let id = spec.id.clone();
        self.sim.create(spec)?;
        self.exec(&id, program, stdio)
    }
    fn signal(&self, id: &BoxId, signal: Signal) -> Result<(), Refusal> {
        self.sim.signal(id, signal)
    }
    fn inspect(&self, id: &BoxId) -> Result<BoxInfo, Refusal> {
        let deadline = Instant::now() + PATIENCE;
        let mut shut = self.shut.lock().unwrap();
        while *shut && Instant::now() < deadline {
            shut = self.opened.wait_timeout(shut, PATIENCE).unwrap().0;
        }
        drop(shut);
        self.sim.inspect(id)
    }
    fn usage(&self, id: &BoxId) -> Result<Usage, Refusal> {
        self.sim.usage(id)
    }
    fn list(&self) -> Result<Vec<BoxInfo>, Refusal> {
        self.sim.list()
    }
    fn delete(&self, id: &BoxId) -> Result<(), Refusal> {
        self.sim.delete(id)
    }
}

impl Host for SimHost {
    /// Box [`g`]'s inspects and execs are its guest's; every other call runs here.
    fn forward(&self, call: Call, stdio: Stdio, reply: Reply, ended: Ended) -> Forward {
        let offered = self.offered.lock().unwrap().clone();
        if matches!(&call, Call::Exec { id, .. } if *id == g())
            && let Some(guest) = offered
        {
            let pid =
                u32::try_from(self.next_pid.fetch_add(1, Ordering::SeqCst)).unwrap() + GUEST_PIDS;
            return guest.offer(pid, reply);
        }
        match call {
            Call::Inspect(id) if id == g() => {
                let info = self.sim.inspect(&id);
                self.guest.answer(Box::new(move || {
                    reply(info.map(|i| Forwarded::Answer(Box::new(Answer::Info(i)))));
                }));
                Forward::Taken
            }
            Call::Exec { id, .. } if id == g() => {
                // A guest pid: no host process has it.
                let pid = u32::try_from(self.next_pid.fetch_add(1, Ordering::SeqCst)).unwrap()
                    + GUEST_PIDS;
                self.guest.programs.lock().unwrap().insert(pid, ended);
                let program = Box::new(GuestProgram {
                    pid,
                    guest: Arc::clone(&self.guest),
                });
                self.guest.answer(Box::new(move || {
                    reply(Ok(Forwarded::Started {
                        pid,
                        program,
                        given: None,
                    }));
                }));
                Forward::Taken
            }
            call => Forward::Back(Box::new((call, stdio))),
        }
    }

    fn abandoned(&self, id: &BoxId) -> Result<(), Refusal> {
        self.abandoned.lock().unwrap().push(id.clone());
        Ok(())
    }
    fn netns(&self) -> Result<netns::Counts, Refusal> {
        Ok(netns::Counts::default())
    }
}

struct Lab {
    os: Arc<SimOs>,
    host: Arc<SimHost>,
    server: Server<Arc<SimOs>>,
}

impl Lab {
    fn new() -> Self {
        Self::with_workers(8)
    }

    fn with_workers(workers: usize) -> Self {
        Self::with_limits(workers, limits())
    }

    fn with_limits(workers: usize, limits: Limits) -> Self {
        Self::reading(workers, limits, &READS)
    }

    /// A lab whose server's list waits as `reads` has learned.
    fn reading(workers: usize, limits: Limits, reads: &'static Patience) -> Self {
        let os = Arc::new(SimOs::default());
        let host = SimHost::new(&os);
        let server = Server::serving(
            Arc::clone(&os),
            Arc::clone(&host) as Arc<dyn Host>,
            limits,
            workers,
            Path::new("/nonexistent/hex-sim.sock"),
            reads,
        )
        .unwrap();
        Self { os, host, server }
    }

    fn connections(&self) -> usize {
        self.server.shared.table.lock().unwrap().conns.len()
    }

    fn watched(&self) -> usize {
        self.server.shared.table.lock().unwrap().watched.len()
    }
}

fn limits() -> Limits {
    Limits {
        connections: 2,
        message_bytes: 4096,
        in_flight: 2,
        programs: 2,
    }
}

fn b() -> BoxId {
    BoxId::new("b").unwrap()
}

/// The box the simulated guest holds.
fn g() -> BoxId {
    BoxId::new("g").unwrap()
}

/// Where the simulated guest's pids start: past any the host gives.
const GUEST_PIDS: u32 = 1 << 20;

fn request(id: u64, call: Call) -> Vec<u8> {
    wire::request(id, &Request::Call(call), wire::MAX_MESSAGE).unwrap()
}

fn hello() -> Vec<u8> {
    let hello = Request::Hello {
        build: wire::build(),
    };
    wire::request(1, &hello, wire::MAX_MESSAGE).unwrap()
}

/// `bytes`, a message of this wire, as the wire whose identity is `theirs` would send it: the identity is the
/// header's bytes 4 to 12 in every build.
fn of_wire(mut bytes: Vec<u8>, theirs: u64) -> Vec<u8> {
    bytes[4..12].copy_from_slice(&theirs.to_le_bytes());
    bytes
}

fn exec(id: u64) -> Vec<u8> {
    exec_in(id, b())
}

/// A run (decision 7) of a program in a `process` box named `name`.
fn run(id: u64, name: &str) -> Vec<u8> {
    request(
        id,
        Call::Run {
            spec: Box::new(BoxSpec {
                route: None,
                allocation: None,
                devices: Vec::new(),
                id: BoxId::new(name).unwrap(),
                bounds: Bounds::default(),
                class: ClassSpec::Process,
                owner: None,
                band: 0,
            }),
            program: Program::of(&["p"]),
            streams: Streams::default(),
            ask: Streams::default(),
        },
    )
}

fn exec_in(id: u64, b: BoxId) -> Vec<u8> {
    request(
        id,
        Call::Exec {
            id: b,
            program: Program::of(&["p"]),
            streams: Streams::default(),
        },
    )
}

/// The next message the server sends `sock`, waiting at most [`PATIENCE`].
fn answer_of(sock: &Sock) -> (Header, Answer) {
    let deadline = Instant::now() + PATIENCE;
    let mut held = sock.sent_lock.lock().unwrap();
    loop {
        if let Some(bytes) = sock.to_client.lock().unwrap().pop_front() {
            let got = Received {
                bytes: &bytes,
                size: bytes.len(),
                descriptors: 0,
                descriptors_cut: false,
            };
            return wire::read_answer(&got, wire::MAX_MESSAGE).unwrap();
        }
        let left = deadline.saturating_duration_since(Instant::now());
        assert!(!left.is_zero(), "no answer came");
        held = sock.sent.wait_timeout(held, left).unwrap().0;
    }
}

fn wait_until(what: &str, mut fact: impl FnMut() -> bool) {
    let deadline = Instant::now() + PATIENCE;
    while !fact() {
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        std::thread::park_timeout(Duration::from_millis(1));
    }
}

#[derive(Debug, PartialEq, Eq)]
enum ProducerStep {
    LockBlocked,
    LockOpen,
    Finished,
}

fn pause_here() -> (
    Box<dyn FnOnce() + Send>,
    std::sync::mpsc::Receiver<()>,
    std::sync::mpsc::SyncSender<()>,
) {
    let (entered, at) = std::sync::mpsc::sync_channel(1);
    let (resume, go) = std::sync::mpsc::sync_channel(1);
    (
        Box::new(move || {
            entered.send(()).unwrap();
            go.recv_timeout(PATIENCE).unwrap();
        }),
        at,
        resume,
    )
}

fn observe_next_lock(os: &Arc<SimOs>, progress: std::sync::mpsc::SyncSender<ProducerStep>) {
    let weak = Arc::downgrade(os);
    os.hooks.lock().unwrap().next_lock = Some(Box::new(move || {
        let os = weak.upgrade().unwrap();
        let step = match os.state.try_lock() {
            Ok(_) => ProducerStep::LockOpen,
            Err(std::sync::TryLockError::WouldBlock) => ProducerStep::LockBlocked,
            Err(error) => panic!("predicate mutex: {error}"),
        };
        progress.send(step).unwrap();
    }));
}

/// HEX-SOCK-3: hold the waiter after its empty scan; a socket producer must join that predicate
/// mutex before its notification can complete. An extra cleanup wake lets the red case terminate
/// immediately, but cannot hide the observed early notification or substitute for the expected frame.
#[test]
fn predicate_wait_protocol_socket_put_cannot_notify_before_sleep_registration() {
    let os = Arc::new(SimOs::default());
    let sock = Sock::new(1);
    os.watch(&Accept(Arc::clone(&sock), true), 7, Interest::Read)
        .unwrap();
    let (hook, at, resume) = pause_here();
    os.hooks.lock().unwrap().before_sleep = Some(hook);
    let waiter = {
        let os = Arc::clone(&os);
        std::thread::spawn(move || {
            let mut events = Vec::new();
            os.wait(&mut events, Some(PATIENCE)).unwrap();
            events
        })
    };
    at.recv_timeout(PATIENCE).unwrap();
    let (progress, steps) = std::sync::mpsc::sync_channel(1);
    observe_next_lock(&os, progress.clone());
    let expected = hello();
    let producer = {
        let os = Arc::clone(&os);
        let sock = Arc::clone(&sock);
        let bytes = expected.clone();
        std::thread::spawn(move || {
            os.put(&sock, bytes);
            progress.send(ProducerStep::Finished).unwrap();
        })
    };
    let first = steps.recv_timeout(PATIENCE).unwrap();
    let early = first != ProducerStep::LockBlocked;
    if early {
        os.hooks.lock().unwrap().next_lock = None;
        resume.send(()).unwrap();
        // Acquisition follows the waiter's atomic condvar unlock, so this rescue cannot itself
        // fall into the demonstrated check-to-sleep gap. It is cleanup, never the asserted wake.
        let _state = os.state.lock().unwrap();
        os.changed.notify_all();
    } else {
        resume.send(()).unwrap();
    }
    let events = waiter.join().unwrap();
    producer.join().unwrap();
    assert!(events.contains(&Event::Ready {
        token: 7,
        readable: true,
        writable: false,
        hung_up: false
    }));
    let mut bytes = vec![0; wire::MAX_MESSAGE];
    match os.recv(&Accept(sock, true), &mut bytes) {
        Recv::Message { got, size, .. } => {
            assert_eq!(size, expected.len());
            assert_eq!(&bytes[..got], expected);
        }
        _ => panic!("the signalled frame was not readable"),
    }
    assert_eq!(
        first,
        ProducerStep::LockBlocked,
        "socket wake finished inside the predicate-to-sleep gap"
    );
}

/// HEX-SOCK-7: end a program between the wait predicate and its sleep. The predicate mutex must
/// remain owned throughout that transition, so the producer cannot finish an unnoticed end there.
#[test]
fn predicate_wait_protocol_program_end_cannot_notify_before_sleep_registration() {
    let os = Arc::new(SimOs::default());
    os.lock().programs.insert(42, (7, false));
    let (hook, at, resume) = pause_here();
    os.hooks.lock().unwrap().after_proc_check = Some(hook);
    let waiter = {
        let os = Arc::clone(&os);
        std::thread::spawn(move || SimProc { pid: 42, os }.wait_for(PATIENCE).unwrap())
    };
    at.recv_timeout(PATIENCE).unwrap();
    let (progress, steps) = std::sync::mpsc::sync_channel(1);
    observe_next_lock(&os, progress.clone());
    let producer = {
        let os = Arc::clone(&os);
        std::thread::spawn(move || {
            os.end(42);
            progress.send(ProducerStep::Finished).unwrap();
        })
    };
    let first = steps.recv_timeout(PATIENCE).unwrap();
    if first == ProducerStep::LockOpen {
        assert_eq!(
            steps.recv_timeout(PATIENCE).unwrap(),
            ProducerStep::Finished
        );
        let (hook, sleeping, go) = pause_here();
        os.hooks.lock().unwrap().before_sleep = Some(hook);
        resume.send(()).unwrap();
        sleeping.recv_timeout(PATIENCE).unwrap();
        go.send(()).unwrap();
        let _state = os.state.lock().unwrap();
        os.changed.notify_all();
    } else {
        resume.send(()).unwrap();
    }
    let exit = waiter.join().unwrap();
    producer.join().unwrap();
    assert_eq!(exit, Some(Exit::Code(0)));
    assert_eq!(
        first,
        ProducerStep::LockBlocked,
        "program end finished inside the predicate-to-sleep gap"
    );
}

/// HEX-SOCK-3 (the core): a hello is answered with the node's build, the host's classes, the server's limits with
/// what it keeps for each connection ([`KEPT`]) as the connection's, and its budgets, over an OS that isn't Linux's.
#[test]
fn a_hello_is_answered_over_any_os() {
    let lab = Lab::new();
    let sock = lab.os.connect(true, 64);
    lab.os.put(&sock, hello());
    match answer_of(&sock) {
        (_, Answer::Hello(h)) => {
            assert_eq!(h.build, wire::build());
            assert_eq!(h.classes, lab.host.sim.classes());
            assert_eq!(
                h.limits,
                Limits {
                    in_flight: KEPT,
                    programs: KEPT,
                    ..limits()
                }
            );
            assert_eq!(
                h.budgets,
                wire::Budgets {
                    in_flight: limits().in_flight,
                    programs: limits().programs,
                }
            );
        }
        other => panic!("{other:?}"),
    }
}

/// One wire (the lead's word of 2026-11-09, which drops the append-only rule), over any OS: a client of another wire
/// is refused at its hello, and nothing more it sends is read. Do: as a client of another wire, a hello (its identity
/// and its build, in the hello's head every build lays out alike) and a delete of box b, both waiting before the server
/// reads; then a client of this wire's hello and inspect of b. Expect: the hello answered with the node's own, and
/// nothing more, its connection shut and let go with the delete unread, b kept; the second greeted and its inspect
/// answered.
#[test]
fn a_client_of_another_wire_is_refused_at_its_hello_over_any_os() {
    let lab = Lab::new();
    let theirs = !wire::IDENTITY;
    let other = Sock::new(64);
    let their_hello = Request::Hello {
        build: "hex 9.9.9 (another wire)".to_owned(),
    };
    other.to_server.lock().unwrap().extend([
        of_wire(
            wire::request(1, &their_hello, wire::MAX_MESSAGE).unwrap(),
            theirs,
        ),
        of_wire(request(2, Call::Delete(b())), theirs),
    ]);
    lab.os.lock().pending.push_back((Arc::clone(&other), true));
    lab.os.poke();
    match answer_of(&other) {
        (Header { id: 1, .. }, Answer::Hello(h)) => assert_eq!(h.build, wire::build()),
        other => panic!("the node's own hello, not {other:?}"),
    }
    wait_until("the refused connection to be let go", || {
        lab.connections() == 0
    });
    assert!(other.shut.load(Ordering::SeqCst), "its connection shut");
    assert!(
        other.to_client.lock().unwrap().is_empty(),
        "nothing more answered"
    );
    assert_eq!(lab.host.sim.inspect(&b()).unwrap().id, b());
    let sock = lab.os.connect(true, 64);
    lab.os.put(&sock, hello());
    assert!(matches!(answer_of(&sock).1, Answer::Hello(_)));
    lab.os.put(&sock, request(2, Call::Inspect(b())));
    assert!(matches!(
        answer_of(&sock),
        (Header { id: 2, .. }, Answer::Info(_))
    ));
}

/// HEX-SOCK-4 (the core): connections past the bound, and a caller the OS says isn't this user, are each
/// refused by type, naming why, and let go; the held ones are served.
#[test]
fn strangers_and_connections_past_the_bound_are_refused_by_name() {
    let lab = Lab::new();
    let stranger = lab.os.connect(false, 64);
    match answer_of(&stranger) {
        (
            _,
            Answer::Refused(Refusal::Invalid {
                what: "caller",
                why,
            }),
        ) => {
            assert!(why.contains("simulated stranger"), "{why}")
        }
        other => panic!("{other:?}"),
    }
    let held: Vec<_> = (0..2).map(|_| lab.os.connect(true, 64)).collect();
    wait_until("both to be admitted", || lab.connections() == 2);
    let third = lab.os.connect(true, 64);
    assert_eq!(
        answer_of(&third).1,
        Answer::Refused(Refusal::Exhausted {
            what: CONNECTIONS,
            bound: 2
        })
    );
    lab.os.put(&held[0], hello());
    assert!(matches!(answer_of(&held[0]).1, Answer::Hello(_)));
}

/// HEX-SOCK-5 (the core): requests past a connection's in-flight bound are refused by type, and every
/// admitted one is answered once its host call returns.
#[test]
fn requests_past_the_in_flight_bound_are_refused_and_none_lost() {
    let lab = Lab::new();
    lab.host.hold(true);
    let sock = lab.os.connect(true, 64);
    for id in 1..=3 {
        lab.os.put(&sock, request(id, Call::Inspect(b())));
    }
    let (h, refused) = answer_of(&sock);
    assert_eq!(
        (h.id, refused),
        (
            3,
            Answer::Refused(Refusal::Exhausted {
                what: IN_FLIGHT,
                bound: 2
            })
        )
    );
    lab.host.hold(false);
    let mut ids: Vec<u64> = (0..2).map(|_| answer_of(&sock).0.id).collect();
    ids.sort_unstable();
    assert_eq!(ids, [1, 2]);
}

/// HEX-SOCK-7 (the core): a program's start is answered before its end, which comes as its exec's next
/// message once the OS's watch says it ended; the watch is then let go.
#[test]
fn a_programs_end_follows_its_start_and_its_watch_is_let_go() {
    let lab = Lab::new();
    let sock = lab.os.connect(true, 64);
    lab.os.put(&sock, exec(7));
    let pid = match answer_of(&sock) {
        (Header { id: 7, .. }, Answer::Started { pid }) => pid,
        other => panic!("{other:?}"),
    };
    assert_eq!(lab.watched(), 1);
    lab.os.end(pid);
    let (h, end) = answer_of(&sock);
    assert_eq!((h.id, end), (7, Answer::Exited(Exit::Code(0))));
    wait_until("the watch to be let go", || {
        lab.watched() == 0 && !lab.os.lock().programs.contains_key(&pid)
    });
}

/// HEX-SOCK-6 (the core), the disconnect rule: a program whose client goes is killed by its own runtime's kill
/// (its `Proc::kill`, which tells its box's bookkeeping first, as macOS's watcher needs), never the OS's
/// watch, reaped and counted in its box; execs past the programs bound are refused naming it.
#[test]
fn a_program_whose_client_goes_is_killed_and_counted_and_the_bound_holds() {
    let lab = Lab::new();
    let sock = lab.os.connect(true, 64);
    for id in 1..=2 {
        lab.os.put(&sock, exec(id));
        assert!(matches!(answer_of(&sock).1, Answer::Started { .. }));
    }
    lab.os.put(&sock, exec(3));
    assert_eq!(
        answer_of(&sock).1,
        Answer::Refused(Refusal::Exhausted {
            what: PROGRAMS,
            bound: 2
        })
    );
    lab.os.leave(&sock);
    wait_until("both programs to be counted in their box", || {
        lab.host.abandoned.lock().unwrap().len() == 2
    });
    let killed = lab.os.lock().proc_killed.len();
    assert_eq!(
        killed, 2,
        "the disconnect rule didn't kill its programs by their own runtime"
    );
    wait_until("the connection and watches to go", || {
        lab.connections() == 0 && lab.watched() == 0
    });
}

/// HEX-SOCK-7 (the core), `Proc::kill` through the socket (HEX-RT-16): a kill naming a program's exec is
/// answered done, kills that program alone by its own runtime's kill (its `Proc::kill`: Windows' ends it with
/// KILL's job code, macOS's tells its box's watcher first), never the OS's watch, and its end, `Signal(9)`,
/// comes as its exec's message, not counted as abandoned; a kill naming no running program of the connection is
/// done too.
#[test]
fn a_kill_ends_its_program_alone_and_its_end_is_sent() {
    let lab = Lab::new();
    let sock = lab.os.connect(true, 64);
    let mut pids = Vec::new();
    for id in [7, 8] {
        lab.os.put(&sock, exec(id));
        match answer_of(&sock) {
            (Header { id: got, .. }, Answer::Started { pid }) if got == id => pids.push(pid),
            other => panic!("{other:?}"),
        }
    }
    lab.os.put(&sock, request(9, Call::Kill { exec: 7 }));
    let mut got = [answer_of(&sock), answer_of(&sock)].map(|(h, a)| (h.id, a));
    got.sort_by_key(|(id, _)| *id);
    assert_eq!(
        got,
        [(7, Answer::Exited(Exit::Signal(9))), (9, Answer::Done)]
    );
    assert_eq!(
        lab.os
            .lock()
            .proc_killed
            .iter()
            .copied()
            .collect::<Vec<_>>(),
        [pids[0]]
    );
    assert!(!lab.os.ended(pids[1]), "the other program was ended");
    lab.os.put(&sock, request(10, Call::Kill { exec: 7 }));
    let (h, again) = answer_of(&sock);
    assert_eq!((h.id, again), (10, Answer::Done));
    assert!(lab.host.abandoned.lock().unwrap().is_empty());
}

/// HEX-SOCK-5 (the core), native plan step 8 (1): a call its guest is slow to answer holds no other client's
/// call, even with one worker: client A's inspect of a guest box waits at the guest while client B's inspect
/// of a host box is answered; A's is answered once the guest answers.
#[test]
fn a_call_its_guest_holds_holds_no_other_clients_call() {
    let lab = Lab::with_workers(1);
    lab.host.guest.hold(true);
    let a = lab.os.connect(true, 64);
    let other = lab.os.connect(true, 64);
    lab.os.put(&a, request(1, Call::Inspect(g())));
    wait_until("the guest to hold A's call", || {
        lab.host.guest.holding() == 1
    });
    lab.os.put(&other, request(2, Call::Inspect(b())));
    let (h, answer) = answer_of(&other);
    assert!(
        matches!((h.id, &answer), (2, Answer::Info(_))),
        "{answer:?}"
    );
    lab.host.guest.hold(false);
    let (h, answer) = answer_of(&a);
    assert!(
        matches!((h.id, &answer), (1, Answer::Info(_))),
        "{answer:?}"
    );
}

/// HEX-SOCK-7 (the core), native plan step 8 (1): a program in the guest has a guest pid, which names no host
/// process, so the core never watches it as one; its end comes from the guest and goes to its exec as any
/// end does.
#[test]
fn a_guest_programs_end_comes_from_its_guest_and_no_host_pid_is_watched() {
    let lab = Lab::new();
    let sock = lab.os.connect(true, 64);
    lab.os.put(&sock, exec_in(7, g()));
    let pid = match answer_of(&sock) {
        (Header { id: 7, .. }, Answer::Started { pid }) => pid,
        other => panic!("{other:?}"),
    };
    assert!(
        !lab.os.lock().ever_watched.contains(&pid),
        "the core watched guest pid {pid} as a host process"
    );
    lab.host.guest.end(pid, Exit::Code(3));
    let (h, end) = answer_of(&sock);
    assert_eq!((h.id, end), (7, Answer::Exited(Exit::Code(3))));
}

/// HEX-SOCK-7 (the core), native plan step 8 (1): a kill of a guest program goes to its guest, is answered
/// once the guest says it was sent, and its end, `Signal(9)`, comes as its exec's message; no host program is
/// killed.
#[test]
fn a_guest_programs_kill_goes_to_its_guest() {
    let lab = Lab::new();
    let sock = lab.os.connect(true, 64);
    lab.os.put(&sock, exec_in(7, g()));
    assert!(matches!(answer_of(&sock).1, Answer::Started { .. }));
    lab.os.put(&sock, request(8, Call::Kill { exec: 7 }));
    let mut got = [answer_of(&sock), answer_of(&sock)].map(|(h, a)| (h.id, a));
    got.sort_by_key(|(id, _)| *id);
    assert_eq!(
        got,
        [(7, Answer::Exited(Exit::Signal(9))), (8, Answer::Done)]
    );
    assert!(lab.os.lock().proc_killed.is_empty());
}

/// HEX-SOCK-6 (the core), native plan step 8 (1), the disconnect rule for a guest program: its client goes,
/// so it is killed through its guest, and its end is counted in its box, not sent.
#[test]
fn a_guest_program_whose_client_goes_is_killed_by_its_guest_and_counted() {
    let lab = Lab::new();
    let sock = lab.os.connect(true, 64);
    lab.os.put(&sock, exec_in(7, g()));
    assert!(matches!(answer_of(&sock).1, Answer::Started { .. }));
    lab.os.leave(&sock);
    wait_until("the guest program to be counted in its box", || {
        lab.host.abandoned.lock().unwrap().as_slice() == [g()]
    });
    wait_until("the connection and its program to go", || {
        lab.connections() == 0 && lab.watched() == 0
    });
    assert!(lab.os.lock().proc_killed.is_empty());
}

/// HEX-SOCK-7 (the core), runtime goal 7 (decision 7): a run's program whose watch reports its end before its runtime
/// can tell it (its node removing its box, which tells the end once the box is gone) has its end waited for on a
/// worker, off the event set: the I/O thread is woken by that watch once and serves the connection meanwhile, and the
/// end is sent once told. Do: run a program, end it with its end untold, inspect a box, then tell the end. Expect: the
/// program's watch let go and reported ready once, the inspect answered while the end is untold, then the run's end,
/// Code(0), and nothing left watched.
#[test]
fn a_runs_end_told_late_is_waited_for_on_a_worker() {
    let lab = Lab::new();
    let sock = lab.os.connect(true, 64);
    lab.os.put(&sock, run(7, "r"));
    let pid = match answer_of(&sock) {
        (Header { id: 7, .. }, Answer::Started { pid }) => pid,
        other => panic!("{other:?}"),
    };
    lab.os.lock().untold.insert(pid);
    lab.os.end(pid);
    wait_until("the untold end's watch to be let go", || {
        !lab.os.lock().programs.contains_key(&pid)
    });
    lab.os.put(&sock, request(8, Call::Inspect(b())));
    assert!(matches!(
        answer_of(&sock),
        (Header { id: 8, .. }, Answer::Info(_))
    ));
    assert_eq!(lab.os.lock().reported.get(&pid), Some(&1));
    lab.os.tell(pid);
    let (h, end) = answer_of(&sock);
    assert_eq!((h.id, end), (7, Answer::Exited(Exit::Code(0))));
    assert_eq!(lab.watched(), 0);
}

/// Decision 9 (original; the person's ruling of 2026-11-13: W* is a bound only while answers come within it): a run's
/// end waited for on a worker (its watch reported before its runtime could tell it) was `Exit::Unknown` at
/// node.emptying's W*, the slowest emptying this process had seen, though its runtime went on to tell it. Do: once an
/// emptying of a microsecond has been seen, run a program, end it with its end untold, and tell the end only once W*,
/// the slowest emptying seen (other tests share it, and may raise it meanwhile), has passed since, or an answer came.
/// Expect: the run's end, Code(0), never Unknown, and nothing left watched.
#[test]
fn original_a_runs_end_told_past_the_emptyings_w_star_is_its_own_never_unknown() {
    crate::emptying::EMPTYING.answered(Duration::from_micros(1));
    let lab = Lab::new();
    let sock = lab.os.connect(true, 64);
    lab.os.put(&sock, run(7, "r"));
    let pid = match answer_of(&sock) {
        (Header { id: 7, .. }, Answer::Started { pid }) => pid,
        other => panic!("{other:?}"),
    };
    lab.os.lock().untold.insert(pid);
    lab.os.end(pid);
    let ended = Instant::now();
    wait_until("W* to pass since the end, or an answer", || {
        let window = crate::emptying::EMPTYING.slowest().unwrap_or_default();
        ended.elapsed() > window || !sock.to_client.lock().unwrap().is_empty()
    });
    lab.os.tell(pid);
    let (h, end) = answer_of(&sock);
    assert_eq!((h.id, end), (7, Answer::Exited(Exit::Code(0))));
    assert_eq!(lab.watched(), 0);
}

/// HEX-SOCK-7 (the core), runtime goal 7 (decision 7): a run's program its node reaped before the core could watch it
/// (to remove its box) is answered as any run is, its start and then its end, which its `Proc` tells, and its place
/// among the programs is given back. Do: run a program the OS finds reaped when the core watches it, then exec two
/// programs, the connection's places. Expect: Started with its pid, then Exited(Code(0)); nothing watched; both
/// execs started.
#[test]
fn a_run_reaped_before_its_watch_is_answered_started_then_ended() {
    let lab = Lab::new();
    let sock = lab.os.connect(true, 64);
    let pid = u32::try_from(lab.host.next_pid.load(Ordering::SeqCst)).unwrap();
    lab.os.lock().reaped.insert(pid);
    lab.os.put(&sock, run(7, "r"));
    match answer_of(&sock) {
        (Header { id: 7, .. }, Answer::Started { pid: started }) => assert_eq!(started, pid),
        other => panic!("{other:?}"),
    }
    let (h, end) = answer_of(&sock);
    assert_eq!((h.id, end), (7, Answer::Exited(Exit::Code(0))));
    assert_eq!(lab.watched(), 0);
    for id in [8, 9] {
        lab.os.put(&sock, exec(id));
        assert!(
            matches!(answer_of(&sock).1, Answer::Started { .. }),
            "exec {id}"
        );
    }
}

/// HEX-SOCK-7 (the core): an exec whose program the OS can't watch is refused, saying the node ended it.
#[test]
fn an_exec_whose_program_cant_be_watched_is_refused() {
    let lab = Lab::new();
    lab.os.lock().unwatchable = true;
    let sock = lab.os.connect(true, 64);
    lab.os.put(&sock, exec(1));
    match answer_of(&sock).1 {
        Answer::Refused(Refusal::Os { op, why }) => {
            assert_eq!(op, "watching a started program");
            assert!(why.contains("ended it"), "{why}");
        }
        other => panic!("{other:?}"),
    }
    assert_eq!(lab.watched(), 0);
}

/// HEX-SOCK-8 (the core): a client that stops reading stops being read: its answers wait for room, and once
/// it has room again they go, and only then are its later requests read.
#[test]
fn a_client_that_doesnt_read_stops_being_read_until_it_has_room() {
    let lab = Lab::new();
    let sock = lab.os.connect(true, 0);
    lab.os.put(&sock, request(1, Call::Inspect(b())));
    lab.os.put(&sock, request(2, Call::Inspect(b())));
    wait_until("both answers to wait for room", || {
        lab.server
            .shared
            .table
            .lock()
            .unwrap()
            .conns
            .values()
            .any(|c| {
                let out = c.out.lock().unwrap();
                out.backlog.len() == 2 && !out.reading
            })
    });
    lab.os.put(&sock, request(3, Call::Inspect(b())));
    lab.os.give_room(&sock, 64);
    let ids: Vec<u64> = (0..3).map(|_| answer_of(&sock).0.id).collect();
    // Calls 1 and 2 ran on two workers, which answer in the order they finish (the wire matches answers
    // to requests by id); 3 wasn't read until their answers had room, so it comes after both.
    let mut first: Vec<u64> = ids.get(..2).unwrap().to_vec();
    first.sort_unstable();
    assert_eq!(
        (first.as_slice(), ids[2]),
        ([1, 2].as_slice(), 3),
        "{ids:?}"
    );
}

/// A lab whose server's list waits as `reads` has learned, its host holding boxes enough for a list of three parts or
/// more; and how many parts its list has.
fn listing(reads: &'static Patience) -> (Lab, usize) {
    let lab = Lab::reading(8, limits(), reads);
    let mut n = 0usize;
    loop {
        let infos = lab.host.sim.list().unwrap();
        let parts = wire::boxes(1, &infos, limits().message_bytes)
            .unwrap()
            .len();
        if parts >= 3 {
            return (lab, parts);
        }
        lab.host
            .sim
            .create(BoxSpec {
                route: None,
                allocation: None,
                devices: Vec::new(),
                id: BoxId::new(&format!("listed-{n}")).unwrap(),
                bounds: Bounds::default(),
                class: ClassSpec::Process,
                owner: None,
                band: 0,
            })
            .unwrap();
        n += 1;
    }
}

/// The parts of a list its client is sent, up to its last, each waited for at most [`PATIENCE`].
fn list_parts(sock: &Sock) -> usize {
    let mut parts = 0;
    loop {
        match answer_of(sock).1 {
            Answer::Boxes { more, .. } => {
                parts += 1;
                if !more {
                    return parts;
                }
            }
            other => panic!("a list's part was {other:?}"),
        }
    }
}

/// Decision 9 (original; the person's ruling of 2026-11-13: W* is a bound only while answers come within it): a
/// client reading a list slower than any client before it had its connection closed at W*, the slowest read seen,
/// though it was there to read. Do: a server whose reads have learned a W* of 1 s; a client with no room asks for a
/// list of three parts or more, and makes room only once the next part's wait has run out (told, the wait following)
/// or its connection has closed. Expect: the connection standing past W*; once the client reads, every part, and W*
/// risen to the read that passed it.
#[test]
fn original_a_client_reading_a_list_slower_than_any_before_is_followed_past_w_star() {
    static READS: Patience = Patience::measuring("test.serve.reads.followed");
    let learned = Duration::from_secs(1);
    READS.answered(learned);
    let (lab, parts) = listing(&READS);
    let sock = lab.os.connect(true, 0);
    lab.os.put(&sock, request(1, Call::List));
    wait_until(
        "the next part's wait to run out, or the client's connection to close",
        || READS.deadline().is_none() || sock.shut.load(Ordering::SeqCst),
    );
    assert!(
        !sock.shut.load(Ordering::SeqCst),
        "the connection was closed at W*, its client there to read"
    );
    lab.os.give_room(&sock, 64);
    assert_eq!(list_parts(&sock), parts);
    assert!(
        READS.slowest().is_some_and(|slowest| slowest > learned),
        "W* didn't rise to the read that passed it: {:?}",
        READS.slowest()
    );
}

/// Decision 9 (the person's ruling of 2026-11-13: W* is a bound only while answers come within it; past it the wait
/// follows its party, the client, by its life alone, as at every local site): a list's client that took nothing over a
/// whole W* window past W* had its connection closed, though it was there to read. Do: a server whose reads have
/// learned a W* of 1 ms; a client with no room asks for a list of three parts or more; once the next part's wait has
/// run out (told), the client makes no room for a hundred windows of W*, or until its connection closes; then it makes
/// room. Expect: the connection standing; once the client reads, every part.
#[test]
fn a_client_past_w_star_is_followed_by_its_life_however_long_it_takes_nothing() {
    static READS: Patience = Patience::measuring("test.serve.reads.life");
    let learned = Duration::from_millis(1);
    READS.answered(learned);
    let (lab, parts) = listing(&READS);
    let sock = lab.os.connect(true, 0);
    lab.os.put(&sock, request(1, Call::List));
    wait_until(
        "the next part's wait to run out, or the client's connection to close",
        || READS.deadline().is_none() || sock.shut.load(Ordering::SeqCst),
    );
    let told = Instant::now();
    while !sock.shut.load(Ordering::SeqCst) && told.elapsed() < learned.saturating_mul(100) {
        std::thread::park_timeout(learned);
    }
    assert!(
        !sock.shut.load(Ordering::SeqCst),
        "the connection was closed past W*, its client there to read"
    );
    lab.os.give_room(&sock, 64);
    assert_eq!(list_parts(&sock), parts);
}

/// The core when the OS runs out of descriptors: accepting pauses, and a connection that closes resumes it,
/// so a waiting client is admitted.
#[test]
fn accepting_pauses_when_descriptors_are_spent_and_resumes_on_a_close() {
    let lab = Lab::new();
    let first = lab.os.connect(true, 64);
    wait_until("the first to be admitted", || lab.connections() == 1);
    lab.os.lock().spent = true;
    let second = lab.os.connect(true, 64);
    wait_until("accepting to pause", || !lab.os.lock().accepting);
    lab.os.lock().spent = false;
    lab.os.leave(&first);
    wait_until("the second to be admitted", || {
        lab.connections() == 1 && lab.os.lock().pending.is_empty()
    });
    lab.os.put(&second, hello());
    assert!(matches!(answer_of(&second).1, Answer::Hello(_)));
}

/// A real Server over SimOs with one worker, a held guest offer and an unrelated live host program.
/// Calls have room beyond the program budget, so the capacity probe reaches PROGRAMS, not IN_FLIGHT.
struct OfferedLab {
    lab: Lab,
    guest: Arc<OfferedGuest>,
    caller: Arc<Sock>,
    other: Arc<Sock>,
    other_pid: u32,
}

impl OfferedLab {
    fn new(failure: OfferFailure) -> Self {
        Self::with_programs(failure, limits().programs)
    }

    fn with_programs(failure: OfferFailure, programs: usize) -> Self {
        let budgets = Limits {
            programs,
            ..limits()
        };
        let lab = Lab::with_limits(
            1,
            Limits {
                in_flight: budgets.programs + budgets.connections,
                ..budgets
            },
        );
        let guest = OfferedGuest::new(failure);
        *lab.host.offered.lock().unwrap() = Some(Arc::clone(&guest));
        let other = lab.os.connect(true, usize::MAX);
        lab.os.put(&other, exec(20));
        let other_pid = match answer_of(&other).1 {
            Answer::Started { pid } => pid,
            answer => panic!("the unrelated program did not start: {answer:?}"),
        };
        let caller = lab.os.connect(true, usize::MAX);
        lab.os.put(&caller, exec_in(7, g()));
        Self {
            lab,
            guest,
            caller,
            other,
            other_pid,
        }
    }

    fn control(&self, sock: &Sock, id: u64, request: Request) {
        self.lab.os.put(
            sock,
            wire::request(id, &request, wire::MAX_MESSAGE).unwrap(),
        );
    }

    /// An inspect queued after control messages is a barrier through the real connection reader.
    fn barrier(&self, sock: &Sock) {
        self.lab.os.put(sock, request(90, Call::Inspect(b())));
        assert!(matches!(
            answer_of(sock),
            (Header { id: 90, .. }, Answer::Info(_))
        ));
    }

    fn offered(&self) {
        assert!(matches!(
            answer_of(&self.caller),
            (Header { id: 7, .. }, Answer::Offered(_))
        ));
        let table = self.lab.server.shared.table.lock().unwrap();
        let conn = table
            .conns
            .values()
            .find(|c| Arc::ptr_eq(&c.sock.0, &self.caller))
            .unwrap();
        let out = conn.out.lock().unwrap();
        assert_eq!(out.reserved, 1);
        assert_eq!(out.in_flight, 1);
        assert_eq!(out.offered.len(), 1);
        assert_eq!(self.lab.server.shared.reserving.load(Ordering::Acquire), 1);
    }

    /// The real pending entry's admission owner, retained across a hostile duplicate outer id.
    fn pending(&self) -> Option<Arc<AtomicBool>> {
        let table = self.lab.server.shared.table.lock().unwrap();
        let conn = table
            .conns
            .values()
            .find(|c| Arc::ptr_eq(&c.sock.0, &self.caller))
            .unwrap();
        conn.out
            .lock()
            .unwrap()
            .offered
            .get(&7)
            .map(|pending| Arc::clone(&pending.completed))
    }

    fn refused(&self) {
        let (h, answer) = answer_of(&self.caller);
        assert_eq!(
            (h.id, answer),
            (7, Answer::Refused(OfferedGuest::refusal()))
        );
    }

    fn reclaimed(&self) {
        assert_eq!(
            self.lab.server.shared.reserving.load(Ordering::Acquire),
            0,
            "the real global reservation count leaked or underflowed"
        );
        assert_eq!(self.lab.server.shared.in_flight.load(Ordering::Acquire), 0);
        let table = self.lab.server.shared.table.lock().unwrap();
        for conn in table.conns.values() {
            let out = conn.out.lock().unwrap();
            assert_eq!(out.reserved, 0);
            assert_eq!(out.in_flight, 0);
            assert!(out.offered.is_empty());
        }
        drop(table);
        assert_eq!(
            self.guest.cancellations.lock().unwrap().len(),
            1,
            "the deferred closure must be cancelled once"
        );
        assert!(
            !self.lab.os.ended(self.other_pid),
            "the other client's program was ended"
        );
        assert_eq!(self.lab.watched(), 1);
    }

    fn stale_controls(&self) {
        for request in [
            Request::StreamsReady,
            Request::Release,
            Request::StreamsReady,
            Request::Release,
        ] {
            self.control(&self.caller, 7, request);
        }
        self.barrier(&self.caller);
    }

    /// Reuse the actual capacity, then disconnect that caller; the unrelated program still runs.
    fn reuse_and_disconnect(&self, caller: &Arc<Sock>) {
        self.lab.os.put(caller, exec(11));
        assert!(matches!(
            answer_of(caller),
            (Header { id: 11, .. }, Answer::Started { .. })
        ));
        assert_eq!(self.lab.watched(), self.lab.server.shared.limits.programs);
        self.lab.os.leave(caller);
        wait_until("the caller and its new program to leave", || {
            self.lab.connections() == 1 && self.lab.watched() == 1
        });
        self.reclaimed();
        // This is the other client's second program, beyond its kept place: admission must use
        // the global capacity the disconnected caller returned, not its own guaranteed first slot.
        self.lab.os.put(&self.other, exec(21));
        assert!(matches!(
            answer_of(&self.other),
            (Header { id: 21, .. }, Answer::Started { .. })
        ));
        assert_eq!(self.lab.watched(), self.lab.server.shared.limits.programs);
        assert!(!self.lab.os.ended(self.other_pid));
    }
}

/// HEX-RT-17 / HEX-SOCK-6: a guest error may run its reply before Host::forward returns Offered.
/// No offer is registered afterward; late Ready/Release and disconnect cannot return its admission twice.
#[test]
fn an_offered_guest_error_before_registration_reclaims_admission_once() {
    let offered = OfferedLab::new(OfferFailure::BeforeRegistration);
    offered.refused();
    offered.stale_controls();
    offered.reclaimed();
    assert!(offered.guest.ready_calls.lock().unwrap().is_empty());
    offered.reuse_and_disconnect(&offered.caller);
}

/// HEX-RT-17 / HEX-SOCK-6: an already-sent Offered is cancelled by the guest's error before Ready.
/// The refusal, closure cancellation and returned program place occur once, leaving the other client live.
#[test]
fn an_offered_guest_error_before_ready_reclaims_admission_once() {
    let offered = OfferedLab::new(OfferFailure::AfterOffered);
    offered.offered();
    offered.guest.fail();
    offered.refused();
    offered.stale_controls();
    offered.reclaimed();
    assert!(offered.guest.ready_calls.lock().unwrap().is_empty());
    offered.reuse_and_disconnect(&offered.caller);
}

/// HEX-RT-17 / HEX-SOCK-6: Ready's send receives the guest's terminal error and itself fails.
/// Both completion paths contend for the same real admission; only the guest refusal is answered.
#[test]
fn an_offered_guest_error_racing_ready_send_failure_reclaims_admission_once() {
    let offered = OfferedLab::new(OfferFailure::DuringReadySend);
    offered.offered();
    offered.control(&offered.caller, 7, Request::StreamsReady);
    offered.refused();
    offered.stale_controls();
    offered.reclaimed();
    assert_eq!(offered.guest.ready_calls.lock().unwrap().len(), 1);
    offered.reuse_and_disconnect(&offered.caller);
}

/// HEX-RT-17 / HEX-SOCK-6: an unsent exec consumes a real program place. Release cancels its deferred
/// closure and frees that place; a late guest error, duplicate Ready/Release and disconnect are harmless.
#[test]
fn an_offered_exec_holds_capacity_until_release_then_reclaims_it_once() {
    let offered = OfferedLab::new(OfferFailure::AfterOffered);
    offered.offered();
    offered.lab.os.put(&offered.caller, exec(8));
    assert_eq!(
        answer_of(&offered.caller).1,
        Answer::Refused(Refusal::Exhausted {
            what: PROGRAMS,
            bound: offered.lab.server.shared.limits.programs,
        })
    );
    offered.control(&offered.caller, 7, Request::Release);
    offered.barrier(&offered.caller);
    offered.guest.fail();
    offered.stale_controls();
    offered.reclaimed();
    assert!(offered.guest.ready_calls.lock().unwrap().is_empty());
    offered.reuse_and_disconnect(&offered.caller);
}

/// HEX-RT-17 / HEX-SOCK-6: disconnect while Offered drops its deferred closure and returns admission.
/// The guest's later error cannot underflow it; another client reuses the actual freed capacity.
#[test]
fn an_offered_exec_disconnected_before_ready_reclaims_capacity_once() {
    let offered = OfferedLab::new(OfferFailure::AfterOffered);
    offered.offered();
    offered.lab.os.leave(&offered.caller);
    wait_until("the offered connection to leave", || {
        offered.lab.connections() == 1
    });
    offered.guest.fail();
    offered.barrier(&offered.other);
    offered.reclaimed();
    assert!(offered.guest.ready_calls.lock().unwrap().is_empty());
    let replacement = offered.lab.os.connect(true, usize::MAX);
    offered.reuse_and_disconnect(&replacement);
}

/// HEX-RT-17 / HEX-SOCK-6: the opposite completion order also returns admission once. Ready's failed
/// send answers first; a later terminal guest error must neither answer again nor underflow the reservation.
#[test]
fn an_offered_ready_send_failure_before_guest_error_reclaims_admission_once() {
    let offered = OfferedLab::new(OfferFailure::ReadySendFailureFirst);
    offered.offered();
    offered.control(&offered.caller, 7, Request::StreamsReady);
    let (h, answer) = answer_of(&offered.caller);
    assert_eq!(
        (h.id, answer),
        (
            7,
            Answer::Refused(Refusal::os(
                "simulated Ready send",
                "the control connection closed",
            ))
        )
    );
    offered.guest.fail();
    offered.stale_controls();
    offered.reclaimed();
    assert_eq!(offered.guest.ready_calls.lock().unwrap().len(), 1);
    offered.reuse_and_disconnect(&offered.caller);
}

/// HEX-RT-17 / HEX-SOCK-6: a second Offered with the same outer exec id cannot replace the first
/// preparation. Only the duplicate's newly acquired place is returned; the original closure stays pending.
#[test]
fn an_offered_duplicate_exec_id_preserves_the_original_pending_offer() {
    let offered = OfferedLab::with_programs(OfferFailure::AfterOffered, limits().programs + KEPT);
    offered.offered();
    let original = offered.pending().unwrap();
    let duplicate = OfferedGuest::new(OfferFailure::AfterOffered);
    *offered.lab.host.offered.lock().unwrap() = Some(Arc::clone(&duplicate));
    offered.lab.os.put(&offered.caller, exec_in(7, g()));
    let (h, answer) = answer_of(&offered.caller);
    offered.barrier(&offered.caller);
    let preserved = offered
        .pending()
        .is_some_and(|pending| Arc::ptr_eq(&pending, &original));
    let original_cancelled = offered.guest.cancellations.lock().unwrap().len();
    let duplicate_cancelled = duplicate.cancellations.lock().unwrap().len();
    let reserving = offered.lab.server.shared.reserving.load(Ordering::Acquire);
    let in_flight = offered.lab.server.shared.in_flight.load(Ordering::Acquire);
    // Complete both callbacks before asserting, so the old broken snapshot leaves no deferred task.
    duplicate.fail();
    offered.guest.fail();
    assert!(
        matches!(
            (h.id, answer),
            (
                7,
                Answer::Refused(Refusal::Invalid {
                    what: "stream offer",
                    ..
                })
            )
        ),
        "the duplicate offer must be refused without replacing its predecessor"
    );
    assert!(
        preserved,
        "the duplicate replaced the original pending admission owner"
    );
    assert_eq!(
        (
            original_cancelled,
            duplicate_cancelled,
            reserving,
            in_flight
        ),
        (0, 1, 1, 1)
    );
    assert!(
        original.load(Ordering::Acquire),
        "the original completion never claimed its place"
    );
    offered.refused();
    offered.stale_controls();
    offered.reclaimed();
    assert!(offered.guest.ready_calls.lock().unwrap().is_empty());
    assert!(duplicate.ready_calls.lock().unwrap().is_empty());
    offered.lab.os.leave(&offered.caller);
    wait_until("the duplicate-id caller to leave", || {
        offered.lab.connections() == 1
    });
    offered.reclaimed();
}

/// HEX-RT-17 / HEX-SOCK-6: a new duplicate's guest error arrives before that new offer registers.
/// Its callback may reclaim its own slot, but cannot remove the older pending entry sharing its header id.
#[test]
fn an_offered_duplicate_guest_error_cannot_remove_the_original_pending_offer() {
    let offered = OfferedLab::with_programs(OfferFailure::AfterOffered, limits().programs + KEPT);
    offered.offered();
    let original = offered.pending().unwrap();
    let duplicate = OfferedGuest::new(OfferFailure::BeforeRegistration);
    *offered.lab.host.offered.lock().unwrap() = Some(Arc::clone(&duplicate));
    offered.lab.os.put(&offered.caller, exec_in(7, g()));
    offered.refused();
    offered.barrier(&offered.caller);
    let preserved = offered
        .pending()
        .is_some_and(|pending| Arc::ptr_eq(&pending, &original));
    let original_cancelled = offered.guest.cancellations.lock().unwrap().len();
    let duplicate_cancelled = duplicate.cancellations.lock().unwrap().len();
    let reserving = offered.lab.server.shared.reserving.load(Ordering::Acquire);
    let in_flight = offered.lab.server.shared.in_flight.load(Ordering::Acquire);
    offered.guest.fail();
    assert!(
        preserved,
        "the duplicate's early error removed the original pending admission owner"
    );
    assert_eq!(
        (
            original_cancelled,
            duplicate_cancelled,
            reserving,
            in_flight
        ),
        (0, 1, 1, 1)
    );
    assert!(original.load(Ordering::Acquire));
    offered.refused();
    offered.stale_controls();
    offered.reclaimed();
    assert!(offered.guest.ready_calls.lock().unwrap().is_empty());
    assert!(duplicate.ready_calls.lock().unwrap().is_empty());
    offered.lab.os.leave(&offered.caller);
    wait_until("the duplicate-error caller to leave", || {
        offered.lab.connections() == 1
    });
    offered.reclaimed();
}

/// HEX-RT-17 / HEX-SOCK-6: the host refuses after arranging a deferred guest callback. Its local
/// refusal owns admission first; the queued guest error must not answer or reclaim that place again.
#[test]
fn an_offered_host_refusal_before_queued_guest_error_reclaims_admission_once() {
    let offered = OfferedLab::new(OfferFailure::RefusedBeforeGuestError);
    let (h, answer) = answer_of(&offered.caller);
    assert_eq!(
        (h.id, answer),
        (7, Answer::Refused(OfferedGuest::host_refusal()))
    );
    offered.barrier(&offered.caller);
    offered.reclaimed();
    offered.guest.fail();
    offered.reclaimed();
    offered.stale_controls();
    offered.reclaimed();
    assert!(offered.guest.ready_calls.lock().unwrap().is_empty());
    offered.reuse_and_disconnect(&offered.caller);
}
