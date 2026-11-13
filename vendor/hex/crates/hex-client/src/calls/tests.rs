//! HEX-RT-17: a caller can reserve a guest exec before opening its named stdio connections, then commit or cancel.

use super::*;
use std::io::{Read, Write};
use std::sync::atomic::{AtomicBool, Ordering};

type Opening = Box<dyn FnOnce(&wire::StreamOffer) + Send>;

struct Memory {
    handle: std::net::UdpSocket,
    outbound: Mutex<Vec<Vec<u8>>>,
    inbound: Mutex<VecDeque<Vec<u8>>>,
    offer: Mutex<Option<Result<(), Refusal>>>,
    reply: Mutex<Option<Answer>>,
    opened: Mutex<Vec<wire::StreamOffer>>,
    program_stdio: Mutex<Option<Stdio>>,
    opening: Mutex<Option<Opening>>,
}

impl Memory {
    fn queue(&self, id: u64, answer: Answer) {
        self.queue_borrowed(id, &answer);
    }

    fn queue_borrowed(&self, id: u64, answer: &Answer) {
        self.inbound
            .lock()
            .unwrap()
            .push_back(wire::answer(id, answer, wire::MAX_MESSAGE).unwrap());
        self.handle.send(&[1]).unwrap();
    }
}

impl Transport for Memory {
    type Reading = ();
    const MAKES_ASKED: bool = true;

    fn answers() -> &'static [Patience; KINDS] {
        static ANSWERS: [Patience; KINDS] =
            [const { Patience::measuring("test.deferred.answer") }; KINDS];
        &ANSWERS
    }

    fn class_answers(class: Class) -> &'static [Patience; BOX_KINDS] {
        static CLASS_ANSWERS: [[Patience; BOX_KINDS]; CLASSES] = [
            class_waits!("test.refusal", "process", measuring),
            class_waits!("test.refusal", "oci", measuring),
            class_waits!("test.refusal", "firecracker", measuring),
            class_waits!("test.refusal", "shards", measuring),
        ];
        by_class(&CLASS_ANSWERS, class)
    }

    fn reading(&self) {}

    fn handle(&self) -> Handle<'_> {
        #[cfg(unix)]
        return std::os::fd::AsFd::as_fd(&self.handle);
        #[cfg(windows)]
        std::os::windows::io::AsSocket::as_socket(&self.handle)
    }

    fn frame(message: Vec<u8>) -> Result<Vec<u8>, Refusal> {
        hex_core::stream::frame(&message, wire::MAX_MESSAGE).map_err(Refusal::from)
    }

    fn recv(&self, _: &mut (), take: &mut Take<'_>) -> Result<bool, Refusal> {
        while self.handle.recv(&mut [0u8; 1]).is_ok() {}
        let mut came = false;
        loop {
            let bytes = self.inbound.lock().unwrap().pop_front();
            let Some(bytes) = bytes else { break };
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
        Ok(came)
    }

    fn try_send(&self, bytes: &[u8], at: &mut usize, lent: &[Lent<'_>]) -> Result<bool, Sent> {
        assert!(lent.is_empty(), "a deferred guest call lends no handles");
        self.outbound.lock().unwrap().push(bytes.to_vec());
        *at = bytes.len();
        if let Some(answer) = self.reply.lock().unwrap().as_ref() {
            let (header, _) = sent_request(bytes);
            self.queue_borrowed(header.id, answer);
        }
        if self.offer.lock().unwrap().is_some() {
            let (header, request) = sent_request(bytes);
            match request {
                Request::Call(Call::Exec { streams, .. }) => {
                    let mut offer = offered(header.id);
                    offer.streams = streams;
                    self.queue(header.id, Answer::Offered(offer));
                }
                Request::StreamsReady => {
                    self.queue(header.id, Answer::Started { pid: 7 });
                    self.queue(header.id, Answer::Exited(Exit::Code(0)));
                }
                _ => {}
            }
        }
        Ok(true)
    }

    fn open_offer(&self, offer: &wire::StreamOffer) -> Result<hex_api::Given, Refusal> {
        self.opened.lock().unwrap().push(offer.clone());
        let opening = self.opening.lock().unwrap().take();
        if let Some(opening) = opening {
            opening(offer);
        }
        let mut stdio = Stdio {
            ask: hex_api::Ask {
                stdin: offer.streams.stdin,
                stdout: offer.streams.stdout,
                stderr: offer.streams.stderr,
            },
            ..Stdio::null()
        };
        let given = stdio.make_asked()?;
        *self.program_stdio.lock().unwrap() = Some(stdio);
        match self.offer.lock().unwrap().as_ref().unwrap() {
            Ok(()) => Ok(given),
            Err(why) => Err(why.clone()),
        }
    }

    fn shutdown(&self) {}
}

fn client() -> Core<Memory> {
    let handle = std::net::UdpSocket::bind((std::net::Ipv4Addr::LOCALHOST, 0)).unwrap();
    handle.connect(handle.local_addr().unwrap()).unwrap();
    handle.set_nonblocking(true).unwrap();
    let client = Core {
        conn: triomphe::Arc::try_new(Conn {
            shared: Shared::try_new(Memory {
                handle,
                outbound: Mutex::new(Vec::new()),
                inbound: Mutex::new(VecDeque::new()),
                offer: Mutex::new(None),
                reply: Mutex::new(None),
                opened: Mutex::new(Vec::new()),
                program_stdio: Mutex::new(None),
                opening: Mutex::new(None),
            })
            .unwrap(),
        })
        .unwrap(),
        classes: Vec::new(),
        limits: Limits {
            connections: 1,
            message_bytes: wire::MAX_MESSAGE,
            in_flight: 2,
            programs: 2,
        },
        budgets: wire::Budgets {
            in_flight: 2,
            programs: 2,
        },
    };
    // This stand-in has already negotiated its hello, including the programs bound on acknowledgements.
    client.conn.shared.owed.lock().unwrap().bound = client.budgets.programs;
    client
}

fn offered(request: u64) -> wire::StreamOffer {
    wire::StreamOffer {
        pid: 11,
        started: 13,
        vsock: std::path::PathBuf::from("mock-vsock"),
        token: 17,
        // A dedicated guest control's exec id need not equal the outer socket request's id.
        exec: request.checked_add(1).unwrap(),
        streams: Streams {
            stdin: true,
            stdout: true,
            stderr: false,
        },
    }
}

fn sent_request(bytes: &[u8]) -> (Header, Request) {
    let mut deframer = hex_core::stream::Deframer::new(wire::MAX_MESSAGE);
    let mut rest = bytes;
    let mut message = None;
    while !rest.is_empty() {
        let (used, got) = deframer.take(rest).unwrap();
        assert!(used > 0);
        rest = &rest[used..];
        if let Some(got) = got {
            assert!(message.replace(got).is_none());
        }
    }
    wire::read_request_apart(&message.unwrap(), wire::MAX_MESSAGE).unwrap()
}

fn exec() -> Call {
    Call::Exec {
        id: BoxId::new("deferred").unwrap(),
        program: Program::of(&["cat"]),
        streams: Streams {
            stdin: true,
            stdout: true,
            stderr: false,
        },
    }
}

/// AC-T7.4.1: factual native RPC preserves unknown counters, refuses the wrong reply type and
/// propagates uncertainty without allocating a program or granting a physical holder.
#[test]
fn physical_report_uses_one_factual_rpc_and_preserves_typed_refusal() {
    let client = client();
    let report = hex_api::PhysicalReport {
        unavailable: Vec::new(),
        authority: "native-store".into(),
        devices: vec![hex_api::PhysicalDevice {
            info: hex_api::DeviceInfo {
                id: hex_api::DeviceId {
                    kind: hex_api::DeviceKind::Gpu,
                    key: u64::MAX,
                },
                model: "native-model".into(),
                size: 4096,
                holder: None,
            },
            busy_per_mille: None,
            memory_used_bytes: Some(0),
        }],
    };
    let transport = &client.conn.shared.transport;
    *transport.reply.lock().unwrap() = Some(Answer::PhysicalReport(report.try_clone().unwrap()));
    assert_eq!(client.physical_report().unwrap(), report);
    assert_eq!(
        sent_request(&transport.outbound.lock().unwrap()[0]).1,
        Request::Call(Call::PhysicalReport)
    );
    *transport.reply.lock().unwrap() = Some(Answer::Done);
    assert!(matches!(client.physical_report(), Err(Refusal::Broken(_))));
    let refusal = Refusal::Lifecycle {
        id: BoxId::new("uncertain-device").unwrap(),
        cause: hex_api::LifecycleCause::Uncertain,
    };
    *transport.reply.lock().unwrap() = Some(Answer::Refused(refusal.clone()));
    assert_eq!(client.physical_report().unwrap_err(), refusal);
    assert_eq!(transport.outbound.lock().unwrap().len(), 3);
    let state = client.conn.shared.state().unwrap();
    assert!(state.calls.is_empty());
    assert!(state.programs.is_empty());
    assert!(state.given.is_empty());
}

/// HEX-RT-17: cancel one unsent exec; expect no outbound request or completions, and only its reservation removed.
#[test]
fn dropping_a_deferred_call_cancels_only_its_reservation_and_sends_nothing() {
    let client = client();
    let (answer, answers) = std::sync::mpsc::channel();
    let (end, ends) = std::sync::mpsc::channel();
    let first = client
        .defer(
            exec(),
            Box::new(move |id, got| answer.send((id, got)).unwrap()),
            Box::new(move |got| end.send(got).unwrap()),
        )
        .unwrap();
    let second = client
        .defer(exec(), Box::new(|_, _| {}), Box::new(|_| {}))
        .unwrap();
    let (first_id, second_id) = (first.id(), second.id());
    let shared = &client.conn.shared;
    {
        let mut state = shared.state().unwrap();
        assert_eq!(state.calls.len(), 2);
        assert_eq!(state.programs.len(), 2);
        // Any ends later prepared under the reservation must be cancelled by the same ownership guard.
        state.given.insert(first_id, hex_api::Given::default());
        state.given.insert(second_id, hex_api::Given::default());
    }
    drop(first);
    {
        let state = shared.state().unwrap();
        assert_eq!(state.calls.keys().copied().collect::<Vec<_>>(), [second_id]);
        assert_eq!(
            state.programs.keys().copied().collect::<Vec<_>>(),
            [second_id]
        );
        assert_eq!(state.given.keys().copied().collect::<Vec<_>>(), [second_id]);
    }
    assert!(shared.transport.outbound.lock().unwrap().is_empty());
    assert_eq!(
        answers.try_recv(),
        Err(std::sync::mpsc::TryRecvError::Disconnected)
    );
    assert_eq!(
        ends.try_recv(),
        Err(std::sync::mpsc::TryRecvError::Disconnected)
    );
    drop(second);
    let state = shared.state().unwrap();
    assert!(state.calls.is_empty());
    assert!(state.programs.is_empty());
    assert!(state.given.is_empty());
}

/// HEX-RT-17: send an exec after reserving its id; expect its exact explicit streams, then its answer and end
/// through the existing receive/delivery core. A guest transport's asks hook must never be needed.
#[test]
fn sending_a_deferred_call_keeps_its_id_and_uses_the_same_answer_and_end_core() {
    let client = client();
    let shared = &client.conn.shared;
    let (answer, answers) = std::sync::mpsc::channel();
    let (end, ends) = std::sync::mpsc::channel();
    let pending = client
        .defer(
            exec(),
            Box::new(move |id, got| answer.send((id, got)).unwrap()),
            Box::new(move |got| end.send(got).unwrap()),
        )
        .unwrap();
    let id = pending.id();
    assert!(shared.transport.outbound.lock().unwrap().is_empty());
    pending.send().unwrap();
    let sent = shared.transport.outbound.lock().unwrap();
    assert_eq!(sent.len(), 1);
    let (header, request) = sent_request(&sent[0]);
    assert_eq!(header.id, id);
    assert_eq!(request, Request::Call(exec()));
    drop(sent);
    {
        let mut inbound = shared.transport.inbound.lock().unwrap();
        inbound
            .push_back(wire::answer(id, &Answer::Started { pid: 7 }, wire::MAX_MESSAGE).unwrap());
        inbound.push_back(
            wire::answer(id, &Answer::Exited(Exit::Code(0)), wire::MAX_MESSAGE).unwrap(),
        );
    }
    assert!(shared.recv().unwrap());
    assert_eq!(
        answers.try_recv().unwrap(),
        (id, Ok(Answer::Started { pid: 7 }))
    );
    assert_eq!(ends.try_recv().unwrap(), Ok(Exit::Code(0)));
    let state = shared.state().unwrap();
    assert!(state.calls.is_empty());
    assert!(state.programs.is_empty());
    assert!(state.given.is_empty());
}

fn close_before_start(deferred: bool) {
    let client = client();
    let (answer, answers) = std::sync::mpsc::channel();
    let (end, ends) = std::sync::mpsc::channel();
    let then: Then = Box::new(move |id, got| answer.send((id, got)).unwrap());
    let ended: Ended = Box::new(move |got| end.send(got).unwrap());
    let (id, pending) = if deferred {
        let pending = client.defer(exec(), then, ended).unwrap();
        (pending.id(), Some(pending))
    } else {
        (
            client.begin(exec(), Stdio::null(), then, ended).unwrap(),
            None,
        )
    };
    let shared = &client.conn.shared;
    let why = Refusal::Broken("the connection ended before start".to_owned());
    shared.close(why.clone());
    shared.close(Refusal::Broken(
        "a later close must not notify twice".to_owned(),
    ));
    assert_eq!(answers.try_recv().unwrap(), (id, Err(why)));
    assert_eq!(
        answers.try_recv(),
        Err(std::sync::mpsc::TryRecvError::Disconnected)
    );
    assert_eq!(
        ends.try_recv(),
        Err(std::sync::mpsc::TryRecvError::Disconnected),
        "an unstarted program has no end callback"
    );
    assert_eq!(
        shared.transport.outbound.lock().unwrap().len(),
        usize::from(!deferred)
    );
    drop(pending);
    let state = shared.state().unwrap();
    assert!(state.calls.is_empty());
    assert!(state.programs.is_empty());
}

/// HEX-RT-17: an event-form exec whose start never came gets its call refusal only, exactly once.
#[test]
fn closing_a_pending_exec_refuses_the_call_without_reporting_a_program_end() {
    close_before_start(false);
}

/// HEX-RT-17: an unsent deferred exec gets its call refusal only, never the end of a program it did not start.
#[test]
fn closing_an_unsent_deferred_exec_refuses_the_call_without_reporting_a_program_end() {
    close_before_start(true);
}

/// HEX-RT-17: once Started came through the core, closing reports its program's lost end exactly once.
#[test]
fn closing_after_started_reports_the_programs_lost_end_once() {
    let client = client();
    let (answer, answers) = std::sync::mpsc::channel();
    let (end, ends) = std::sync::mpsc::channel();
    let id = client
        .begin(
            exec(),
            Stdio::null(),
            Box::new(move |id, got| answer.send((id, got)).unwrap()),
            Box::new(move |got| end.send(got).unwrap()),
        )
        .unwrap();
    let shared = &client.conn.shared;
    shared
        .transport
        .inbound
        .lock()
        .unwrap()
        .push_back(wire::answer(id, &Answer::Started { pid: 7 }, wire::MAX_MESSAGE).unwrap());
    assert!(shared.recv().unwrap());
    assert_eq!(
        answers.try_recv().unwrap(),
        (id, Ok(Answer::Started { pid: 7 }))
    );
    assert_eq!(ends.try_recv(), Err(std::sync::mpsc::TryRecvError::Empty));
    let why = Refusal::Broken("the connection ended after start".to_owned());
    shared.close(why.clone());
    shared.close(why.clone());
    assert_eq!(
        ends.try_recv().unwrap(),
        Err(Refusal::Broken(format!(
            "the connection to the node agent closed before the program's end came: {why}"
        )))
    );
    assert_eq!(
        ends.try_recv(),
        Err(std::sync::mpsc::TryRecvError::Disconnected)
    );
    assert_eq!(
        answers.try_recv(),
        Err(std::sync::mpsc::TryRecvError::Disconnected)
    );
    let state = shared.state().unwrap();
    assert!(state.calls.is_empty());
    assert!(state.programs.is_empty());
}

/// HEX-RT-17: a late offer for an expired call is cancelled by encoded Release, while another pending call and
/// a started program still receive their own replies. The read queues its debt instead of writing recursively.
#[test]
fn a_late_offer_is_released_without_closing_other_calls_or_programs() {
    let client = client();
    let shared = &client.conn.shared;
    let mut expired = Caller::new(shared, Some(Arc::new(Ending::default()))).unwrap();
    let expired_id = expired.id;
    let bytes = Memory::frame(
        wire::request(expired_id, &Request::Call(exec()), wire::MAX_MESSAGE).unwrap(),
    )
    .unwrap();
    assert!(expired.send(&bytes, NONE, None, None).is_ok());
    assert!(expired.wait(Some(Instant::now())).is_none());
    drop(expired);

    let missing = BoxId::new("still-pending").unwrap();
    let (answer, answers) = std::sync::mpsc::channel();
    let pending = client
        .defer(
            Call::Inspect(missing.clone()),
            Box::new(move |id, got| answer.send((id, got)).unwrap()),
            Box::new(|_| panic!("an inspect has no program end")),
        )
        .unwrap();
    let pending_id = pending.id();
    pending.send().unwrap();
    let (start, starts) = std::sync::mpsc::channel();
    let (end, ends) = std::sync::mpsc::channel();
    let active = client
        .defer(
            exec(),
            Box::new(move |id, got| start.send((id, got)).unwrap()),
            Box::new(move |got| end.send(got).unwrap()),
        )
        .unwrap();
    let active_id = active.id();
    active.send().unwrap();
    shared
        .transport
        .queue(active_id, Answer::Started { pid: 7 });
    assert!(shared.recv().unwrap());
    assert_eq!(
        starts.try_recv().unwrap(),
        (active_id, Ok(Answer::Started { pid: 7 }))
    );
    let before = shared.transport.outbound.lock().unwrap().len();
    shared
        .transport
        .queue(expired_id, Answer::Offered(offered(expired_id)));
    assert!(shared.recv().unwrap());
    assert!(!client.closed());
    assert!(shared.transport.opened.lock().unwrap().is_empty());
    assert_eq!(
        answers.try_recv(),
        Err(std::sync::mpsc::TryRecvError::Empty)
    );
    assert_eq!(ends.try_recv(), Err(std::sync::mpsc::TryRecvError::Empty));
    assert!(shared.owing());
    assert_eq!(shared.transport.outbound.lock().unwrap().len(), before);
    shared.send_owed_now();
    let outbound = shared.transport.outbound.lock().unwrap();
    assert_eq!(outbound.len(), before + 1);
    let (header, request) = sent_request(outbound.last().unwrap());
    assert_eq!(header.id, expired_id);
    assert_eq!(request, Request::Release);
    drop(outbound);

    shared.transport.queue(
        pending_id,
        Answer::Refused(Refusal::NoSuchBox(missing.clone())),
    );
    shared
        .transport
        .queue(active_id, Answer::Exited(Exit::Code(3)));
    assert!(shared.recv().unwrap());
    assert_eq!(
        answers.try_recv().unwrap(),
        (pending_id, Ok(Answer::Refused(Refusal::NoSuchBox(missing))))
    );
    assert_eq!(ends.try_recv().unwrap(), Ok(Exit::Code(3)));
    assert!(!client.closed());
    let state = shared.state().unwrap();
    assert!(state.calls.is_empty());
    assert!(state.programs.is_empty());
}

/// HEX-RT-17: Offered keeps the original call pending. Opening its ends then sends same-id Ready, and only
/// that acknowledgement lets the mock return Started/Exited through the existing synchronous core reader.
#[test]
fn an_offer_is_intermediate_until_same_id_ready_and_the_programs_real_answer() {
    let client = client();
    let shared = &client.conn.shared;
    *shared.transport.offer.lock().unwrap() = Some(Ok(()));
    // The callback keeps a Weak test witness, never a strong cycle through Shared.
    let observer = Arc::new(triomphe::Arc::clone(shared));
    let weak = Arc::downgrade(&observer);
    *shared.transport.opening.lock().unwrap() = Some(Box::new(move |offer| {
        let shared = weak.upgrade().unwrap();
        let state = shared
            .state
            .try_lock()
            .expect("open_offer must run outside State's lock");
        assert!(!state.reading, "the dial must not retain the reader's turn");
        let outbound = shared.transport.outbound.lock().unwrap();
        assert_eq!(outbound.len(), 1, "Ready must follow opening");
        let (header, request) = sent_request(&outbound[0]);
        assert_ne!(header.id, offer.exec);
        assert!(
            state.calls.contains_key(&header.id),
            "the offer must not remove the waiting call"
        );
        assert!(state.programs.contains_key(&header.id));
        assert!(!state.calls.get(&header.id).unwrap().reads);
        assert_eq!(request, Request::Call(exec()));
    }));
    let ending = Arc::new(Ending::default());
    let (id, answer) = client
        .conn
        .call(
            wire::MAX_MESSAGE,
            &Request::Call(exec()),
            |_| Ok(NONE),
            Some(Arc::clone(&ending)),
        )
        .unwrap();
    assert_eq!(answer, Answer::Started { pid: 7 });
    assert_eq!(
        shared.transport.opened.lock().unwrap().as_slice(),
        [offered(id)]
    );
    let outbound = shared.transport.outbound.lock().unwrap();
    assert_eq!(outbound.len(), 2);
    let (ready, request) = sent_request(&outbound[1]);
    assert_eq!(ready.id, id);
    assert_eq!(request, Request::StreamsReady);
    drop(outbound);
    assert_eq!(ending.state.lock().unwrap().exit, Some(Ok(Exit::Code(0))));

    let given = client
        .given(id)
        .expect("the opened ends must survive Started");
    let mut program = shared
        .transport
        .program_stdio
        .lock()
        .unwrap()
        .take()
        .unwrap();
    let mut stdin = hex_api::OwnedStream::from(given.stdin.unwrap());
    stdin.write_all(b"caller").unwrap();
    drop(stdin);
    let mut input = String::new();
    program
        .stdin
        .take()
        .unwrap()
        .read_to_string(&mut input)
        .unwrap();
    assert_eq!(input, "caller");
    program
        .stdout
        .take()
        .unwrap()
        .write_all(b"program")
        .unwrap();
    let mut output = String::new();
    hex_api::OwnedStream::from(given.stdout.unwrap())
        .read_to_string(&mut output)
        .unwrap();
    assert_eq!(output, "program");
    client.release(id);
    assert!(shared.state().unwrap().calls.is_empty());
    assert!(shared.state().unwrap().programs.is_empty());
}

/// HEX-RT-17: a mocked partial stream-open failure closes its made ends, sends Release rather than Ready, and
/// forgets the local waiting call, streams and program reservation while leaving the connection usable.
#[test]
fn a_failed_offer_open_releases_the_remote_preparation_and_local_reservation() {
    let client = client();
    let shared = &client.conn.shared;
    let why = Refusal::Broken("the caller could not finish opening its offered streams".to_owned());
    *shared.transport.offer.lock().unwrap() = Some(Err(why.clone()));
    let got = client.conn.call(
        wire::MAX_MESSAGE,
        &Request::Call(exec()),
        |_| Ok(NONE),
        Some(Arc::new(Ending::default())),
    );
    assert_eq!(got, Err(why));
    let opened = shared.transport.opened.lock().unwrap();
    assert_eq!(opened.len(), 1);
    let outbound = shared.transport.outbound.lock().unwrap();
    assert_eq!(outbound.len(), 2);
    let id = sent_request(&outbound[0]).0.id;
    assert_ne!(id, opened[0].exec);
    let (header, request) = sent_request(&outbound[1]);
    assert_eq!(header.id, id);
    assert_eq!(request, Request::Release);
    let mut program = shared
        .transport
        .program_stdio
        .lock()
        .unwrap()
        .take()
        .unwrap();
    assert_eq!(
        program.stdin.take().unwrap().read(&mut [0u8; 1]).unwrap(),
        0
    );
    assert!(!client.closed());
    let state = shared.state().unwrap();
    assert!(state.calls.is_empty());
    assert!(state.programs.is_empty());
    assert!(state.given.is_empty());
}

/// AC-T7.4.1: a Release cut short on a stream (`Conn::send_now_before`). The rest of its frame follows with nothing
/// between its bytes, neither an owed word nor another message, or the connection ends.
mod release_frame {
    use super::*;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::mpsc;

    /// How much of one write the agent's socket takes, in the order writes come; once the script is spent, all.
    enum Room {
        /// This many bytes, then no room.
        Part(usize),
        /// None: the agent, not reading while its answers wait for room, writes this answer to this request.
        Stall(u64, Box<Answer>),
        /// This many bytes, then the write fails.
        Fail(usize),
    }

    /// What another reader does while a write cut short is under way.
    type During = Box<dyn FnOnce() + Send>;
    /// An answer as the agent writes it: its bytes, and the ends riding with it.
    type Written = (Vec<u8>, Vec<hex_api::End>);
    /// Each event-form exec's answer, by its id.
    type Answers = mpsc::Receiver<(u64, Result<Answer, Refusal>)>;

    /// A node agent's stream, read as its agent reads it: frame by frame, an exec started and a start done.
    struct Narrow {
        /// Its readiness: a wake written is readable once written, on macOS too, where a loopback
        /// datagram can arrive after its send returns, after the core's retry for room.
        handle: Wake,
        room: Mutex<VecDeque<Room>>,
        during: Mutex<Option<During>>,
        inbound: Mutex<VecDeque<Written>>,
        agent: Mutex<Agent>,
        /// The connection's end: the client's shutdown, or the agent's on bytes it can't read.
        ended: AtomicBool,
    }

    /// The agent's reading of what was written.
    struct Agent {
        frames: hex_core::stream::Deframer,
        /// Each whole request, in order.
        heard: Vec<(u64, Request)>,
        /// The rest of a frame cut short: the next bytes written must be these.
        rest: Vec<u8>,
        /// Why it read no further.
        fault: Option<String>,
    }

    impl Narrow {
        /// Writes the agent's `answer` to request `id`, with an end for each stream a given answer names.
        fn answer(&self, id: u64, answer: &Answer) {
            let ends = match answer {
                Answer::Given { given, .. } => (0..given.count())
                    .map(|_| std::io::pipe().unwrap().0.into())
                    .collect(),
                _ => Vec::new(),
            };
            let bytes = wire::answer(id, answer, wire::MAX_MESSAGE).unwrap();
            self.inbound.lock().unwrap().push_back((bytes, ends));
            self.handle.wake();
        }

        fn end(&self) {
            self.ended.store(true, Ordering::Release);
            self.handle.wake();
        }

        /// The agent reads one write's `bytes`: each request heard, and answered, once its frame is whole.
        fn hear(&self, agent: &mut Agent, mut bytes: &[u8]) {
            while !bytes.is_empty() && agent.fault.is_none() {
                let (used, message) = match agent.frames.take(bytes) {
                    Ok(taken) => taken,
                    Err(fault) => {
                        agent.fault = Some(format!("an unreadable frame: {fault}"));
                        return;
                    }
                };
                bytes = &bytes[used..];
                let Some(message) = message else { continue };
                match wire::read_request_apart(&message, wire::MAX_MESSAGE) {
                    Ok((header, request)) => {
                        match &request {
                            Request::Call(Call::Exec { .. }) => {
                                self.answer(header.id, &Answer::Started { pid: 7 });
                            }
                            Request::Call(Call::Start(_)) => self.answer(header.id, &Answer::Done),
                            _ => {}
                        }
                        agent.heard.push((header.id, request));
                    }
                    Err(bad) => agent.fault = Some(format!("an unreadable request: {bad:?}")),
                }
            }
        }
    }

    impl Transport for Narrow {
        type Reading = ();

        fn answers() -> &'static [Patience; KINDS] {
            static ANSWERS: [Patience; KINDS] =
                [const { Patience::measuring("test.release_frame.answer") }; KINDS];
            &ANSWERS
        }

        fn reading(&self) {}

        fn handle(&self) -> Handle<'_> {
            self.handle.handle()
        }

        fn frame(message: Vec<u8>) -> Result<Vec<u8>, Refusal> {
            hex_core::stream::frame(&message, wire::MAX_MESSAGE).map_err(Refusal::from)
        }

        fn recv(&self, _: &mut (), take: &mut Take<'_>) -> Result<bool, Refusal> {
            self.handle.drain();
            let mut came = false;
            loop {
                let written = self.inbound.lock().unwrap().pop_front();
                let Some((bytes, ends)) = written else { break };
                let message = Received {
                    size: bytes.len(),
                    bytes: &bytes,
                    descriptors: ends.len(),
                    descriptors_cut: false,
                };
                take(&message, ends)?;
                came = true;
            }
            if self.ended.load(Ordering::Acquire) {
                return Err(Refusal::Broken(
                    "the node agent's connection ended".to_owned(),
                ));
            }
            Ok(came)
        }

        fn try_send(&self, bytes: &[u8], at: &mut usize, lent: &[Lent<'_>]) -> Result<bool, Sent> {
            assert!(lent.is_empty(), "these messages lend nothing");
            if self.ended.load(Ordering::Acquire) {
                return Err(Sent::Gone);
            }
            let rest = &bytes[*at..];
            let room = self.room.lock().unwrap().pop_front();
            let (took, fails) = match room {
                None => (rest.len(), false),
                Some(Room::Part(n)) => (n.min(rest.len()), false),
                Some(Room::Fail(n)) => (n.min(rest.len()), true),
                Some(Room::Stall(id, answer)) => {
                    self.answer(id, &answer);
                    (0, false)
                }
            };
            {
                let mut agent = self.agent.lock().unwrap();
                // The transport's contract: no other message's bytes come between one message's own.
                if !agent.rest.is_empty() && rest != agent.rest.as_slice() {
                    agent.fault = Some(format!(
                        "{} bytes of another message came inside a frame {} bytes short",
                        rest.len(),
                        agent.rest.len()
                    ));
                }
                if took > 0 {
                    agent.rest = rest[took..].to_vec();
                }
                self.hear(&mut agent, &rest[..took]);
                if agent.fault.is_some() {
                    self.end();
                }
            }
            *at += took;
            if took < rest.len() {
                let during = self.during.lock().unwrap().take();
                if let Some(during) = during {
                    during();
                }
            }
            if fails {
                return Err(Sent::Refused(Refusal::Broken(
                    "the agent's socket failed under a write".to_owned(),
                )));
            }
            Ok(*at == bytes.len())
        }

        fn shutdown(&self) {
            self.end();
        }
    }

    /// A client of a `Narrow` stream whose agent's budget of programs is `programs`, as its hello would say.
    fn narrow(programs: usize) -> Core<Narrow> {
        let transport = Narrow {
            handle: Wake::new().unwrap(),
            room: Mutex::new(VecDeque::new()),
            during: Mutex::new(None),
            inbound: Mutex::new(VecDeque::new()),
            agent: Mutex::new(Agent {
                frames: hex_core::stream::Deframer::new(wire::MAX_MESSAGE),
                heard: Vec::new(),
                rest: Vec::new(),
                fault: None,
            }),
            ended: AtomicBool::new(false),
        };
        let client = Core {
            conn: triomphe::Arc::try_new(Conn {
                shared: Shared::try_new(transport).unwrap(),
            })
            .unwrap(),
            classes: Vec::new(),
            limits: Limits {
                connections: 1,
                message_bytes: wire::MAX_MESSAGE,
                in_flight: 4,
                programs,
            },
            budgets: wire::Budgets {
                in_flight: 4,
                programs,
            },
        };
        client.conn.shared.owed.lock().unwrap().bound = programs;
        client
    }

    const STDOUT: Streams = Streams {
        stdin: false,
        stdout: true,
        stderr: false,
    };

    fn running() -> BoxId {
        BoxId::new("cut-release").unwrap()
    }

    fn exec_a() -> Request {
        Request::Call(Call::Exec {
            id: running(),
            program: Program::of(&["cat"]),
            streams: Streams::default(),
        })
    }

    /// An event-form exec asking for its program's stdout, whose answer gives that end.
    fn asking() -> Call {
        Call::ExecStreams {
            id: running(),
            program: Program::of(&["cat"]),
            streams: Streams::default(),
            ask: STDOUT,
        }
    }

    fn gave() -> Answer {
        Answer::Given {
            pid: 8,
            given: STDOUT,
        }
    }

    /// Exec A started, its `Proc` returned, then `others` event-form execs asking for stdout sent: their ids, and
    /// where their answers come.
    fn started(client: &Core<Narrow>, others: usize) -> (Box<dyn Proc>, Vec<u64>, Answers) {
        let process = client
            .exec(&running(), Program::of(&["cat"]), Stdio::null())
            .unwrap();
        let (answer, answers) = mpsc::channel();
        let ids = (0..others)
            .map(|_| {
                let answer = answer.clone();
                let other = client
                    .defer(
                        asking(),
                        Box::new(move |id, got| answer.send((id, got)).unwrap()),
                        Box::new(|_| {}),
                    )
                    .unwrap();
                let id = other.id();
                other.send().unwrap();
                id
            })
            .collect();
        (process, ids, answers)
    }

    /// A Release's frame's length: its cut points are 1 up to it.
    fn release_frame() -> usize {
        Narrow::frame(wire::request(0, &Request::Release, wire::MAX_MESSAGE).unwrap())
            .unwrap()
            .len()
    }

    /// AC-T7.4.1: do drop a started program's `Proc` while the agent's socket takes only part of its Release (cut at
    /// each byte of the frame), the agent then writing, not reading, an answer that gives an end, for which the client
    /// owes a Taken word, and make a call after; expect the agent to read the Release, the Taken word and the call,
    /// each whole and in that order, and the call answered on a connection still up.
    #[test]
    fn a_release_cut_short_goes_whole_before_owed_words_and_later_calls() {
        for cut in 1..release_frame() {
            let client = narrow(2);
            let shared = &client.conn.shared;
            let (process, others, answers) = started(&client, 1);
            let other = others[0];
            shared
                .transport
                .room
                .lock()
                .unwrap()
                .extend([Room::Part(cut), Room::Stall(other, Box::new(gave()))]);
            drop(process);
            let start = client.start(&running());
            let agent = shared.transport.agent.lock().unwrap();
            assert_eq!(agent.fault, None, "cut at {cut} of the Release's frame");
            let exec = agent.heard[0].0;
            let ids: Vec<u64> = agent.heard.iter().map(|(id, _)| *id).collect();
            let requests: Vec<&Request> = agent.heard.iter().map(|(_, request)| request).collect();
            assert_eq!(
                requests,
                [
                    &exec_a(),
                    &Request::Call(asking()),
                    &Request::Release,
                    &Request::Taken,
                    &Request::Call(Call::Start(running())),
                ],
                "cut at {cut}"
            );
            assert_eq!(ids[1..4], [other, exec, other], "cut at {cut}");
            assert!(agent.frames.at_boundary(), "cut at {cut}");
            drop(agent);
            assert_eq!(start, Ok(()), "cut at {cut}");
            assert!(!client.closed());
            assert!(!shared.owing());
            assert_eq!(answers.try_recv().unwrap(), (other, Ok(gave())));
        }
    }

    /// Has another reader, while the next write is cut short, read what the agent wrote meanwhile: exec A's end, its
    /// program ending as it is let go, and a given answer to each of `others`, for which the client owes a Taken
    /// word. `observer` holds the connection for that read.
    fn read_while_cut(observer: &Arc<triomphe::Arc<Shared<Narrow>>>, others: &[u64]) {
        let weak = Arc::downgrade(observer);
        let exec = observer.transport.agent.lock().unwrap().heard[0].0;
        let given = others.to_vec();
        *observer.transport.during.lock().unwrap() = Some(Box::new(move || {
            let shared = weak.upgrade().unwrap();
            shared
                .transport
                .answer(exec, &Answer::Exited(Exit::Code(0)));
            for id in given {
                shared.transport.answer(id, &gave());
            }
            shared.recv().unwrap();
        }));
    }

    /// AC-T7.4.1: do drop a started program's `Proc` while the agent's socket takes only part of its Release, other
    /// readers meanwhile owing Taken words within their bound, then make a call; expect the agent to read the Release,
    /// then those words, then the call, each whole.
    #[test]
    fn a_release_cut_short_goes_before_words_owed_while_it_was_written() {
        for cut in 1..release_frame() {
            let client = narrow(3);
            let shared = &client.conn.shared;
            let (process, others, answers) = started(&client, 2);
            let observer = Arc::new(triomphe::Arc::clone(shared));
            read_while_cut(&observer, &others);
            shared
                .transport
                .room
                .lock()
                .unwrap()
                .push_back(Room::Part(cut));
            drop(process);
            let start = client.start(&running());
            let agent = shared.transport.agent.lock().unwrap();
            assert_eq!(agent.fault, None, "cut at {cut} of the Release's frame");
            let exec = agent.heard[0].0;
            let ids: Vec<u64> = agent.heard.iter().map(|(id, _)| *id).collect();
            let requests: Vec<&Request> = agent.heard.iter().map(|(_, request)| request).collect();
            assert_eq!(
                requests,
                [
                    &exec_a(),
                    &Request::Call(asking()),
                    &Request::Call(asking()),
                    &Request::Release,
                    &Request::Taken,
                    &Request::Taken,
                    &Request::Call(Call::Start(running())),
                ],
                "cut at {cut}"
            );
            assert_eq!(ids[3..6], [exec, others[0], others[1]], "cut at {cut}");
            assert!(agent.frames.at_boundary(), "cut at {cut}");
            drop(agent);
            assert_eq!(start, Ok(()), "cut at {cut}");
            assert!(!client.closed());
            assert!(!shared.owing());
            assert_eq!(
                answers.try_iter().collect::<Vec<_>>(),
                [(others[0], Ok(gave())), (others[1], Ok(gave()))]
            );
        }
    }

    /// Cuts a Release short, at each byte of its frame, where its rest can't follow: with `overfull`, other readers
    /// owe the agent its whole budget of words as it goes out; else the socket fails under it. The connection must
    /// end with the cut Release its last bytes, nothing after it for the agent to misread, and the next call refused
    /// by that end.
    fn rest_cannot_follow(overfull: bool) {
        for cut in 1..release_frame() {
            let client = narrow(2);
            let shared = &client.conn.shared;
            let (process, others, _answers) = started(&client, if overfull { 2 } else { 0 });
            let observer = Arc::new(triomphe::Arc::clone(shared));
            let room = if overfull {
                read_while_cut(&observer, &others);
                Room::Part(cut)
            } else {
                Room::Fail(cut)
            };
            shared.transport.room.lock().unwrap().push_back(room);
            drop(process);
            let owed = shared.owed.lock().unwrap().words.len();
            let start = client.start(&running());
            let agent = shared.transport.agent.lock().unwrap();
            assert_eq!(agent.fault, None, "cut at {cut} of the Release's frame");
            let requests: Vec<&Request> = agent.heard.iter().map(|(_, request)| request).collect();
            let mut expected = vec![exec_a()];
            expected.extend(others.iter().map(|_| Request::Call(asking())));
            assert_eq!(
                requests,
                expected.iter().collect::<Vec<_>>(),
                "cut at {cut}"
            );
            assert!(
                !agent.frames.at_boundary(),
                "cut at {cut}: the cut Release is the stream's last bytes"
            );
            drop(agent);
            assert_eq!(
                owed,
                others.len(),
                "cut at {cut}: the words owed fill the bound"
            );
            assert!(
                shared.transport.ended.load(Ordering::Acquire),
                "cut at {cut}"
            );
            assert!(
                matches!(start, Err(Refusal::Broken(_))),
                "cut at {cut}: {start:?}"
            );
            assert!(client.closed(), "cut at {cut}");
        }
    }

    /// AC-T7.4.1: do cut a Release short while other readers owe the agent its whole budget of words; expect the
    /// connection to end with nothing after the cut Release, never those words inside it.
    #[test]
    fn a_release_cut_short_with_the_owed_words_at_their_bound_ends_the_connection() {
        rest_cannot_follow(true);
    }

    /// AC-T7.4.1: do fail the socket part way through a Release; expect the connection to end with nothing after the
    /// cut Release, never the next call's bytes inside it.
    #[test]
    fn a_release_whose_write_fails_part_way_ends_the_connection() {
        rest_cannot_follow(false);
    }
}

/// AC-T7.4.1: the plain kinds' waits keep their order, the scheduler's and a physical report's after a hello, a list
/// and the network namespaces' counts; a call naming a box or a class is no plain kind's, its box kind's wait its
/// class's ([`BoxKind`]). Do: map each plain kind and each box kind's request. Expect: each plain kind at its place
/// in a table of 18, the person's release of a capacity holder appended last; each box kind's none there, and its own
/// kind and class.
#[test]
fn plain_kinds_keep_their_order_and_box_kinds_are_their_classs() {
    let waits = <Memory as Transport>::answers();
    assert_eq!((KINDS, BOX_KINDS, CLASSES), (18, 11, 4));
    for (request, index) in [
        (
            Request::Hello {
                build: String::new(),
            },
            0,
        ),
        (Request::Call(Call::List), 1),
        (Request::Call(Call::Netns), 2),
        (
            Request::Call(Call::Schedule(Box::new(
                hex_core::schedule::Command::Describe {
                    from: 0,
                    cluster: "pool".into(),
                },
            ))),
            3,
        ),
        (
            Request::Call(Call::Schedule(Box::new(
                hex_core::schedule::Command::Status {
                    key: hex_core::schedule::Key {
                        owner: 0,
                        epoch: 1,
                        seq: 0,
                    },
                },
            ))),
            12,
        ),
        (Request::Call(Call::PhysicalReport), 13),
        (
            Request::Call(Call::Schedule(Box::new(
                hex_core::schedule::Command::Forget {
                    holder: "kiln".into(),
                },
            ))),
            17,
        ),
    ] {
        assert!(
            std::ptr::eq(kind_of(waits, &request).unwrap(), &waits[index]),
            "{request:?}"
        );
    }
    let b = BoxId::new("b").unwrap();
    for (request, kind) in [
        (Request::Call(Call::Start(b.clone())), BoxKind::Start),
        (Request::Call(Call::Delete(b.clone())), BoxKind::Delete),
        (Request::Call(Call::Kill { exec: 7 }), BoxKind::Kill),
        (
            Request::Call(Call::Capacity(Class::Shards)),
            BoxKind::Capacity,
        ),
    ] {
        assert!(kind_of(waits, &request).is_none(), "{request:?}");
        assert_eq!(box_kind_of(&request).map(|(kind, _)| kind), Some(kind));
    }
}

/// AC-T7.4.1: each (kind, class) has its own census name: `<prefix>.<kind>.<class>`, none shared with another wait
/// of this platform's native transport or its guest's. Do: read every name of their plain and class tables. Expect:
/// each class table's names its kinds' and its class's, in order, and no name twice.
#[test]
fn every_kind_and_class_has_its_own_census_name() {
    #[cfg(target_os = "linux")]
    type Native = crate::linux::Socket;
    #[cfg(target_os = "macos")]
    type Native = crate::macos::Socket;
    #[cfg(windows)]
    type Native = crate::windows::Socket;
    fn names<T: Transport>(prefix: &str, seen: &mut BTreeSet<&'static str>) {
        let kinds = [
            "create",
            "start",
            "exec",
            "signal",
            "inspect",
            "usage",
            "delete",
            "kill",
            "exec_streams",
            "run",
            "capacity",
        ];
        for wait in T::answers() {
            assert!(seen.insert(wait.name()), "{} twice", wait.name());
        }
        for class in [
            Class::Process,
            Class::Oci,
            Class::Firecracker,
            Class::Shards,
        ] {
            for (kind, wait) in kinds.iter().zip(T::class_answers(class)) {
                assert_eq!(wait.name(), format!("{prefix}.{kind}.{}", class.name()));
                assert!(seen.insert(wait.name()), "{} twice", wait.name());
            }
        }
    }
    let mut seen = BTreeSet::new();
    names::<Native>("client.answer", &mut seen);
    names::<crate::guest::Stream>("client.guest.answer", &mut seen);
    assert_eq!(seen.len(), 2 * (KINDS + CLASSES * BOX_KINDS));
}

/// AC-T7.4.1: pool lifecycle waits append after all existing transport operation seeds.
#[test]
fn backing_lifecycle_waits_append_without_reordering_old_operations() {
    let key = hex_api::BackingKey {
        pool: [1; 32],
        generation: [2; 16],
    };
    let fixed = hex_api::CapacityResources {
        cpu_milli: hex_api::MIN_CPU_MILLI.into(),
        mem_bytes: hex_api::MIN_MEMORY,
        disk_bytes: 0,
    };
    let cost = hex_api::RuntimeCost {
        id: [3; 32],
        floor: fixed,
        ceiling: fixed,
        quantum: hex_api::CapacityResources {
            cpu_milli: 1,
            mem_bytes: 1,
            disk_bytes: 1,
        },
        guest_overhead: hex_api::CapacityResources::default(),
        overhead: hex_api::CapacityResources::default(),
    };
    let grant = hex_api::BackingGrant {
        key,
        allocation: cost
            .allocate(hex_api::CapacityResources::default())
            .unwrap(),
        devices: Vec::new(),
    };
    let waits = <Memory as Transport>::answers();
    assert!(std::ptr::eq(
        kind_of(
            waits,
            &Request::Call(Call::InitializeBacking(triomphe::Arc::new(grant)))
        )
        .unwrap(),
        &waits[14]
    ));
    assert!(std::ptr::eq(
        kind_of(waits, &Request::Call(Call::ReleaseBacking(key))).unwrap(),
        &waits[15]
    ));
    assert!(std::ptr::eq(
        kind_of(waits, &Request::Call(Call::BackingUsage(key))).unwrap(),
        &waits[16]
    ));
}

/// AC-T7.4.1: the common synchronous timing point distinguishes an actual typed wire refusal.
#[test]
fn refusal_census_common_call_records_the_typed_wire_outcome() {
    static REFUSED: AtomicBool = AtomicBool::new(false);
    fn sink(name: &'static str, sample: hex_core::patience::Sample) {
        if name == "test.refusal.capacity.oci"
            && matches!(sample, hex_core::patience::Sample::Refused(_))
        {
            REFUSED.store(true, Ordering::SeqCst);
        }
    }
    assert!(hex_core::patience::census(sink));
    let client = client();
    let expected = Refusal::Exhausted {
        what: "scheduler command publication",
        bound: 1,
    };
    *client.conn.shared.transport.reply.lock().unwrap() = Some(Answer::Refused(expected.clone()));
    assert_eq!(client.capacity(Class::Oci), Err(expected));
    assert!(REFUSED.load(Ordering::SeqCst));
    let patience = &<Memory as Transport>::class_answers(Class::Oci)[BoxKind::Capacity as usize];
    assert!(
        patience.slowest().is_some(),
        "refusals still update online learning"
    );
    assert_eq!(
        patience.deadline(),
        None,
        "a measuring wait remains measuring"
    );
}

#[cfg(target_os = "macos")]
mod queued_deadline_probe {
    use super::*;
    use crate::macos::{Client, Socket};
    use hex_core::{schedule, stream};
    use std::os::unix::net::{UnixListener, UnixStream};

    #[derive(Clone, Copy)]
    enum Queued {
        Complete,
        Decoded,
        PrefixOnly,
        Absent,
    }

    fn next_native_request(peer: &mut UnixStream) -> (Header, Request) {
        let mut prefix = [0u8; size_of::<u32>()];
        peer.read_exact(&mut prefix).unwrap();
        let length = usize::try_from(u32::from_be_bytes(prefix)).unwrap();
        assert!(length <= wire::MAX_MESSAGE);
        let mut bytes = prefix.to_vec();
        bytes.resize(prefix.len() + length, 0);
        peer.read_exact(&mut bytes[prefix.len()..]).unwrap();
        sent_request(&bytes)
    }

    fn framed_answer(id: u64, answer: &Answer) -> Vec<u8> {
        stream::frame(
            &wire::answer(id, answer, wire::MAX_MESSAGE).unwrap(),
            wire::MAX_MESSAGE,
        )
        .unwrap()
    }

    /// No Runtime is invoked: the stand-in negotiates the real native transport and echoes a typed reply.
    fn expired_native_submit(queued: Queued) -> Option<Result<Answer, Refusal>> {
        let cache = std::path::PathBuf::from(std::env::var_os("HOME").unwrap())
            .join(".cache/hex-t7/client");
        std::fs::create_dir_all(&cache).unwrap();
        let directory = tempfile::Builder::new()
            .prefix("qd-")
            .tempdir_in(&cache)
            .unwrap();
        let path = directory.path().join("node.sock");
        let listener = UnixListener::bind(&path).unwrap();
        let request = Request::Call(Call::Schedule(Box::new(schedule::Command::Submit {
            demand: "class:oci".into(),
            payload: schedule::Payload {
                spec: BoxSpec {
                    route: None,
                    allocation: None,
                    devices: Vec::new(),
                    id: BoxId::new("queued-deadline-probe").unwrap(),
                    bounds: hex_api::Bounds::default(),
                    class: hex_api::ClassSpec::Bundle(directory.path().join("unused-bundle")),
                    owner: None,
                },
                start: true,
            },
        })));
        let expected_request = wire::request(0, &request, wire::MAX_MESSAGE).unwrap();
        let answer = Answer::Scheduled(Box::new(schedule::Reply {
            key: Some(schedule::Key {
                owner: 0,
                epoch: 1,
                seq: 0,
            }),
            tracked: true,
            created: true,
            ..schedule::Reply::default()
        }));
        let (written, queued_bytes) = std::sync::mpsc::sync_channel(1);
        let (release, held) = std::sync::mpsc::sync_channel(1);
        let agent = std::thread::spawn(move || {
            let (mut peer, _) = listener.accept().unwrap();
            let (header, hello) = next_native_request(&mut peer);
            assert_eq!(
                hello,
                Request::Hello {
                    build: wire::build()
                }
            );
            let limits = Limits {
                connections: 1,
                message_bytes: wire::MAX_MESSAGE,
                in_flight: 1,
                programs: 1,
            };
            peer.write_all(&framed_answer(
                header.id,
                &Answer::Hello(wire::Hello {
                    build: wire::build(),
                    classes: vec![Class::Oci],
                    limits,
                    budgets: wire::Budgets {
                        in_flight: 1,
                        programs: 1,
                    },
                }),
            ))
            .unwrap();
            let (header, submitted) = next_native_request(&mut peer);
            assert_eq!(
                wire::request(0, &submitted, wire::MAX_MESSAGE).unwrap(),
                expected_request
            );
            let frame = framed_answer(header.id, &answer);
            match queued {
                Queued::Complete | Queued::Decoded => peer.write_all(&frame).unwrap(),
                Queued::PrefixOnly => peer.write_all(&frame[..size_of::<u32>()]).unwrap(),
                Queued::Absent => {}
            }
            // This event is after the actual complete/partial write, never before it.
            written.send((header.id, answer)).unwrap();
            held.recv().unwrap(); // Keep the real peer open until Caller::wait has returned.
        });
        let client = Client::connect(&path).unwrap();
        let shared = client.shared();
        let mut caller = Caller::new(shared, None).unwrap();
        let frame =
            Socket::frame(wire::request(caller.id, &request, wire::MAX_MESSAGE).unwrap()).unwrap();
        assert!(caller.send(&frame, NONE, None, None).is_ok());
        let (response_id, expected) = queued_bytes.recv().unwrap();
        let observed = ready(shared.transport.handle(), false, None, Some(Duration::ZERO));
        let decoded = if matches!(queued, Queued::Decoded) {
            // The sole registered caller owns the otherwise free reader turn, as wait() would claim it.
            let mut state = shared.state().unwrap();
            assert!(!state.reading);
            state.reading = true;
            caller.mine = true;
            drop(state);
            assert!(shared.recv().unwrap());
            !shared.state().unwrap().calls.contains_key(&caller.id)
        } else {
            false
        };
        // Evaluated after the write-completion event: at wait entry this deadline cannot be in the future.
        let deadline = Instant::now();
        let actual = caller.wait(Some(deadline));
        let request_id = caller.id;
        drop(caller);
        release.send(()).unwrap();
        agent.join().unwrap();
        assert_eq!(response_id, request_id);
        assert_eq!(observed.unwrap().read, !matches!(queued, Queued::Absent));
        if matches!(queued, Queued::Decoded) {
            assert!(
                decoded,
                "the real framed reply must have reached the caller's mailbox before wait"
            );
        }
        if let Some(Ok(answer)) = &actual {
            assert_eq!(answer, &expected);
        }
        actual
    }

    /// AC-T7.4.1: queue a complete native Submit reply before an expired wait; expect immediate completion,
    /// as for an already decoded reply. Root's unchanged production baseline returns None here. This does
    /// not establish the cause of the earlier physical Submit timeout.
    #[test]
    fn cp_t7_4_complete_submit_reply_queued_before_expired_native_wait_is_available() {
        assert!(matches!(
            expired_native_submit(Queued::Complete),
            Some(Ok(Answer::Scheduled(_)))
        ));
    }

    /// AC-T7.4.1: the identical native reply, decoded by the owned reader before wait, is collected even
    /// with an expired deadline. Contrast raw queued bytes without changing its message or peer lifetime.
    #[test]
    fn cp_t7_4_decoded_submit_reply_is_collected_before_expired_native_deadline() {
        assert!(matches!(
            expired_native_submit(Queued::Decoded),
            Some(Ok(Answer::Scheduled(_)))
        ));
    }

    /// AC-T7.4.1: readable length bytes alone are not a complete reply and must not extend an expired wait.
    #[test]
    fn cp_t7_4_incomplete_native_reply_does_not_extend_expired_wait() {
        assert!(expired_native_submit(Queued::PrefixOnly).is_none());
    }

    /// AC-T7.4.1: no queued bytes and an open peer yield no answer at an already expired deadline.
    #[test]
    fn cp_t7_4_absent_native_reply_does_not_extend_expired_wait() {
        assert!(expired_native_submit(Queued::Absent).is_none());
    }
}
