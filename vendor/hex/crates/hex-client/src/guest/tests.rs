//! The guest client's tests (docs/design/shards-guest.md §3, HEX-GUEST-4 and HEX-GUEST-5; native plan
//! step 5, serving HEX-RT-1..13 through the seam): against a stand-in agent on loopback, the client says
//! hello with its build, then calls, framed, in its one wire; an exec's stdin and stdout
//! ride connections of their own, named for it, and its end comes back; and an agent that sends bytes
//! that aren't a message, or closes mid-message, refuses every waiting call by type.

use std::io::{Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::sync::Arc;
use std::time::Duration;

use super::*;
use hex_api::{BoxId, Class, Exit, Program, Runtime};
use hex_core::wire::{Answer, Call, Header, Limits, Request};

/// The guest's ports, as two loopback listeners.
struct Loopback {
    control: SocketAddr,
    stdio: SocketAddr,
}

impl Link for Loopback {
    fn dial(&self, port: u32) -> Result<DuplexOwner, Refusal> {
        let addr = match port {
            stream::CONTROL_PORT => self.control,
            stream::STDIO_PORT => self.stdio,
            other => panic!("dialed port {other:#x}"),
        };
        own_duplex(TcpStream::connect(addr).unwrap())
    }

    /// TCP carries a half-close (RFC 9293 §3.6).
    fn carries_half_close(&self) -> bool {
        true
    }
}

fn listeners() -> (TcpListener, TcpListener, LinkOwner) {
    let control = TcpListener::bind("127.0.0.1:0").unwrap();
    let stdio = TcpListener::bind("127.0.0.1:0").unwrap();
    let link = own_link(Loopback {
        control: control.local_addr().unwrap(),
        stdio: stdio.local_addr().unwrap(),
    })
    .unwrap();
    (control, stdio, link)
}

/// AC-T7.4.1: owning a link makes no dial; clones retain the same link until the last drop.
#[test]
fn link_owner_retains_one_instance_without_dial_effects() {
    use std::sync::atomic::{AtomicUsize, Ordering};

    struct Counted {
        dials: Arc<AtomicUsize>,
        drops: Arc<AtomicUsize>,
    }
    impl Link for Counted {
        fn dial(&self, _: u32) -> Result<DuplexOwner, Refusal> {
            self.dials.fetch_add(1, Ordering::SeqCst);
            Err(Refusal::Broken("owned test dial".into()))
        }
    }
    impl Drop for Counted {
        fn drop(&mut self) {
            self.drops.fetch_add(1, Ordering::SeqCst);
        }
    }

    let dials = Arc::new(AtomicUsize::new(0));
    let drops = Arc::new(AtomicUsize::new(0));
    let owner = own_link(Counted {
        dials: Arc::clone(&dials),
        drops: Arc::clone(&drops),
    })
    .unwrap();
    let retained = owner.clone();
    assert!(triomphe::Arc::ptr_eq(&owner, &retained));
    assert_eq!(dials.load(Ordering::SeqCst), 0);
    assert_eq!(drops.load(Ordering::SeqCst), 0);
    assert!(matches!(
        retained.dial(stream::CONTROL_PORT),
        Err(Refusal::Broken(_))
    ));
    assert_eq!(dials.load(Ordering::SeqCst), 1);
    drop(owner);
    assert_eq!(drops.load(Ordering::SeqCst), 0);
    drop(retained);
    assert_eq!(drops.load(Ordering::SeqCst), 1);
}

/// The next framed request on `conn`, waiting on the fact: its bytes, or its end, which fails the test.
fn next_request(conn: &mut TcpStream) -> (Header, Request) {
    let mut d = Deframer::new(wire::MAX_MESSAGE);
    let mut byte = [0u8; 1];
    loop {
        conn.read_exact(&mut byte).unwrap();
        if let (_, Some(m)) = d.take(&byte).unwrap() {
            return wire::read_request_apart(&m, wire::MAX_MESSAGE).unwrap();
        }
    }
}

fn send_answer(conn: &mut TcpStream, id: u64, answer: &Answer) {
    let bytes = wire::answer(id, answer, wire::MAX_MESSAGE).unwrap();
    conn.write_all(&stream::frame(&bytes, wire::MAX_MESSAGE).unwrap())
        .unwrap();
}

/// The relay's token for every stand-in connection.
const TOKEN: u64 = 0x7465_7374;

/// The stand-in agent's hello, of this wire and this build.
fn agents_hello() -> wire::Hello {
    wire::Hello {
        build: wire::build(),
        classes: vec![Class::Oci],
        // The stand-in's own limits, inputs rather than tuning: past what any test here makes.
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
    }
}

fn hello(conn: &mut TcpStream) -> Header {
    conn.write_all(&TOKEN.to_le_bytes()).unwrap();
    let (h, request) = next_request(conn);
    assert_eq!(
        request,
        Request::Hello {
            build: wire::build()
        }
    );
    send_answer(conn, h.id, &Answer::Hello(agents_hello()));
    h
}

/// HEX-GUEST-4: connect, then inspect a box the agent doesn't hold: expect the hello and the inspect, each
/// framed, the classes the agent named, and its refusal returned typed.
#[test]
fn a_call_goes_framed_and_its_refusal_comes_back_typed() {
    let (control, _stdio, link) = listeners();
    let agent = std::thread::spawn(move || {
        let (mut conn, _) = control.accept().unwrap();
        let first = hello(&mut conn);
        let (h, request) = next_request(&mut conn);
        let Request::Call(Call::Inspect(id)) = request else {
            panic!("expected an inspect, got {request:?}");
        };
        send_answer(&mut conn, h.id, &Answer::Refused(Refusal::NoSuchBox(id)));
        [first.kind, h.kind]
    });
    let client = Client::connect(link).unwrap();
    assert_eq!(client.classes(), vec![Class::Oci]);
    let b = BoxId::new("b").unwrap();
    assert_eq!(client.inspect(&b), Err(Refusal::NoSuchBox(b)));
    assert_eq!(agent.join().unwrap(), [0x0001, 0x0006]);
}

/// One wire (the lead's word of 2026-11-09, which drops the append-only rule), over the guest's stream: a client
/// refuses an agent of another wire at the hello, typed, naming both wires' identities and both builds, and sends it
/// nothing more; an agent of its own wire it serves. Do: connect to a stand-in agent that answers the hello, framed,
/// as an agent of another wire would (its own identity and build, in the hello's head every build lays out alike);
/// then connect again, answered in this wire, and inspect a box. Expect: the first connect refused as
/// `Invalid { what: "wire" }` naming both, its connection ended with nothing more sent; the second greeted and its
/// inspect answered.
#[test]
fn an_agent_of_another_wire_is_refused_at_the_hello() {
    let (control, _stdio, link) = listeners();
    let theirs = !wire::IDENTITY;
    let their_build = "hex 9.9.9 (another wire)";
    let agent = std::thread::spawn(move || {
        let (mut conn, _) = control.accept().unwrap();
        conn.write_all(&TOKEN.to_le_bytes()).unwrap();
        let (h, request) = next_request(&mut conn);
        assert!(matches!(request, Request::Hello { .. }), "{request:?}");
        let mut bytes = wire::answer(
            h.id,
            &Answer::Hello(wire::Hello {
                build: their_build.to_owned(),
                ..agents_hello()
            }),
            wire::MAX_MESSAGE,
        )
        .unwrap();
        bytes[4..12].copy_from_slice(&theirs.to_le_bytes());
        conn.write_all(&stream::frame(&bytes, wire::MAX_MESSAGE).unwrap())
            .unwrap();
        // Nothing more comes: the client's end of the connection, read as its close.
        let mut rest = Vec::new();
        conn.read_to_end(&mut rest).unwrap();
        let (mut conn, _) = control.accept().unwrap();
        hello(&mut conn);
        let (h, request) = next_request(&mut conn);
        let Request::Call(Call::Inspect(id)) = request else {
            panic!("an inspect, not {request:?}")
        };
        send_answer(&mut conn, h.id, &Answer::Refused(Refusal::NoSuchBox(id)));
        (rest, conn)
    });
    let refused = Client::connect(link.clone()).err();
    let Some(Refusal::Invalid { what: "wire", why }) = &refused else {
        panic!("refused for its wire, not {refused:?}")
    };
    for named in [
        format!("{theirs:016x}"),
        format!("{:016x}", wire::IDENTITY),
        their_build.to_owned(),
        wire::build(),
    ] {
        assert!(why.contains(&named), "{why:?} names {named:?}");
    }
    let client = Client::connect(link).unwrap();
    let b = BoxId::new("b").unwrap();
    assert_eq!(client.inspect(&b), Err(Refusal::NoSuchBox(b.clone())));
    let (rest, conn) = agent.join().unwrap();
    assert!(
        rest.is_empty(),
        "the refused client sent {} bytes after its hello",
        rest.len()
    );
    drop(conn);
}

/// HEX-GUEST-4: exec a program with stdin and stdout whose agent echoes one into the other: expect two
/// stdio connections named for the exec's id, the bytes written to stdin read back from stdout to its
/// end, and the program's end as the agent sent it.
#[test]
fn an_execs_stdio_rides_named_connections_and_its_end_comes_back() {
    let (control, stdio, link) = listeners();
    let agent = std::thread::spawn(move || {
        let (mut conn, _) = control.accept().unwrap();
        hello(&mut conn);
        let (h, request) = next_request(&mut conn);
        let Request::Call(Call::Exec { streams, .. }) = request else {
            panic!("expected an exec, got {request:?}");
        };
        assert!(streams.stdin && streams.stdout && !streams.stderr);
        let mut named = std::collections::BTreeMap::new();
        for _ in 0..2 {
            let (mut s, _) = stdio.accept().unwrap();
            let mut name = [0u8; stream::NAME_BYTES];
            s.read_exact(&mut name).unwrap();
            let n = stream::read_name(&name).unwrap();
            assert_eq!(
                n.exec,
                Exec {
                    conn: TOKEN,
                    id: h.id
                },
                "a stream names its connection's token and its exec's request id"
            );
            named.insert(n.stream, s);
        }
        send_answer(&mut conn, h.id, &Answer::Started { pid: 42 });
        let mut stdin = named.remove(&stream::Stdio::In).unwrap();
        let mut stdout = named.remove(&stream::Stdio::Out).unwrap();
        std::io::copy(&mut stdin, &mut stdout).unwrap();
        drop(stdout);
        send_answer(&mut conn, h.id, &Answer::Exited(Exit::Code(0)));
        // Held open until the client has its end, as an agent's connection is.
        let _ = next_request(&mut conn);
    });
    let client = Client::connect(link).unwrap();
    let (in_r, mut in_w) = std::io::pipe().unwrap();
    let (mut out_r, out_w) = std::io::pipe().unwrap();
    let mut proc = client
        .exec(
            &BoxId::new("b").unwrap(),
            Program::of(&["cat"]),
            Stdio {
                stdin: Some(in_r),
                stdout: Some(out_w),
                stderr: None,
                ..Stdio::null()
            },
        )
        .unwrap();
    assert_eq!(proc.pid(), 42);
    in_w.write_all(b"hello through the guest\n").unwrap();
    drop(in_w);
    let mut out = String::new();
    out_r.read_to_string(&mut out).unwrap();
    assert_eq!(out, "hello through the guest\n");
    assert_eq!(proc.wait_for(Duration::MAX).unwrap(), Some(Exit::Code(0)));
    drop(proc);
    let _ = client.inspect(&BoxId::new("b").unwrap());
    agent.join().unwrap();
}

/// HEX-GUEST-5: have the agent announce a frame past the wire's bound while a call waits: expect the
/// call refused by type, naming bytes that aren't a message, before any body is read.
#[test]
fn an_agent_announcing_an_oversized_frame_is_refused() {
    let (control, _stdio, link) = listeners();
    let agent = std::thread::spawn(move || {
        let (mut conn, _) = control.accept().unwrap();
        hello(&mut conn);
        let _ = next_request(&mut conn);
        let past = u32::try_from(wire::MAX_MESSAGE + 1).unwrap();
        conn.write_all(&past.to_be_bytes()).unwrap();
        conn
    });
    let client = Client::connect(link).unwrap();
    let got = client.inspect(&BoxId::new("b").unwrap());
    let Err(Refusal::Broken(why)) = got else {
        panic!("expected a broken connection, got {got:?}");
    };
    assert!(why.contains("aren't a message"), "{why}");
    drop(agent.join().unwrap());
}

/// HEX-GUEST-5: have the agent close the connection partway through an answer: expect the waiting call
/// refused by type, naming the cut message.
#[test]
fn an_agent_closing_mid_message_is_refused() {
    let (control, _stdio, link) = listeners();
    let agent = std::thread::spawn(move || {
        let (mut conn, _) = control.accept().unwrap();
        hello(&mut conn);
        let (h, _) = next_request(&mut conn);
        let bytes = wire::answer(h.id, &Answer::Done, wire::MAX_MESSAGE).unwrap();
        let framed = stream::frame(&bytes, wire::MAX_MESSAGE).unwrap();
        conn.write_all(&framed[..framed.len() - 1]).unwrap();
    });
    let client = Client::connect(link).unwrap();
    let got = client.start(&BoxId::new("b").unwrap());
    agent.join().unwrap();
    let Err(Refusal::Broken(why)) = got else {
        panic!("expected a broken connection, got {got:?}");
    };
    assert!(why.contains("middle of a message"), "{why}");
}

/// How a connection's sends are bounded: by a timeout on its socket, or by the client's core, which writes it without
/// waiting and waits on it within its call's learned wait.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Bound {
    Socket(Option<Duration>),
    Core,
}

/// Each send bound set on a connection, by the port it was dialed on.
type SendBounds = Arc<std::sync::Mutex<Vec<(u32, Bound)>>>;

/// A connection that records each send bound set on it, by the port it was dialed on.
struct Recorded {
    conn: TcpStream,
    port: u32,
    bounds: SendBounds,
}

impl Read for Recorded {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        self.conn.read(buf)
    }
}

impl Write for Recorded {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.conn.write(buf)
    }

    fn flush(&mut self) -> std::io::Result<()> {
        self.conn.flush()
    }
}

impl Duplex for Recorded {
    fn try_clone(&self) -> std::io::Result<DuplexOwner> {
        own_duplex(Recorded {
            conn: self.conn.try_clone()?,
            port: self.port,
            bounds: Arc::clone(&self.bounds),
        })
        .map_err(|_| io::Error::from(io::ErrorKind::OutOfMemory))
    }

    fn shutdown(&self, how: Shutdown) -> std::io::Result<()> {
        self.conn.shutdown(how)
    }

    fn set_write_timeout(&self, timeout: Option<Duration>) -> std::io::Result<()> {
        self.bounds
            .lock()
            .unwrap()
            .push((self.port, Bound::Socket(timeout)));
        self.conn.set_write_timeout(timeout)
    }

    fn set_read_timeout(&self, timeout: Option<Duration>) -> std::io::Result<()> {
        self.conn.set_read_timeout(timeout)
    }

    fn handle(&self) -> super::Handle<'_> {
        Duplex::handle(&self.conn)
    }

    fn set_nonblocking(&self, on: bool) -> std::io::Result<()> {
        if on {
            self.bounds.lock().unwrap().push((self.port, Bound::Core));
        }
        self.conn.set_nonblocking(on)
    }
}

struct Recording {
    control: SocketAddr,
    stdio: SocketAddr,
    bounds: SendBounds,
}

impl Link for Recording {
    fn dial(&self, port: u32) -> Result<DuplexOwner, Refusal> {
        let addr = if port == stream::CONTROL_PORT {
            self.control
        } else {
            self.stdio
        };
        own_duplex(Recorded {
            conn: TcpStream::connect(addr).unwrap(),
            port,
            bounds: Arc::clone(&self.bounds),
        })
    }
}

/// HEX-GUEST-32 (§3, CLAUDE.md §8): exec a program with stdin through the guest: expect its stdio
/// connection given no send bound, as a local exec's stdin is the caller's own pipe, which waits on the
/// program as long as it runs; only the control connection is bounded, as the local client's socket is: written
/// without waiting and waited on by the client's core, within its call's learned wait.
#[test]
fn a_stdio_connection_waits_on_its_program_as_a_local_pipe_does() {
    let (control, stdio) = (
        TcpListener::bind("127.0.0.1:0").unwrap(),
        TcpListener::bind("127.0.0.1:0").unwrap(),
    );
    let bounds = Arc::new(std::sync::Mutex::new(Vec::new()));
    let link = own_link(Recording {
        control: control.local_addr().unwrap(),
        stdio: stdio.local_addr().unwrap(),
        bounds: Arc::clone(&bounds),
    })
    .unwrap();
    let agent = std::thread::spawn(move || {
        let (mut conn, _) = control.accept().unwrap();
        hello(&mut conn);
        let (h, _) = next_request(&mut conn);
        let (s, _) = stdio.accept().unwrap();
        send_answer(&mut conn, h.id, &Answer::Started { pid: 7 });
        send_answer(&mut conn, h.id, &Answer::Exited(Exit::Code(0)));
        let _ = next_request(&mut conn);
        drop(s);
    });
    let client = Client::connect(link).unwrap();
    let (in_r, in_w) = std::io::pipe().unwrap();
    let mut proc = client
        .exec(
            &BoxId::new("b").unwrap(),
            Program::of(&["cat"]),
            Stdio {
                stdin: Some(in_r),
                stdout: None,
                stderr: None,
                ..Stdio::null()
            },
        )
        .unwrap();
    assert_eq!(proc.wait_for(Duration::MAX).unwrap(), Some(Exit::Code(0)));
    drop((proc, in_w));
    let _ = client.inspect(&BoxId::new("b").unwrap());
    agent.join().unwrap();
    let bounds = bounds.lock().unwrap().clone();
    assert!(
        bounds.iter().all(|(port, _)| *port == stream::CONTROL_PORT),
        "{bounds:?}"
    );
    assert!(
        bounds.contains(&(stream::CONTROL_PORT, Bound::Core)),
        "{bounds:?}"
    );
}

/// A stream over loopback, as Shards' is over its device.
struct Looped(TcpStream);

impl Read for Looped {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        self.0.read(buf)
    }
}

impl Write for Looped {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0.write(buf)
    }

    fn flush(&mut self) -> std::io::Result<()> {
        self.0.flush()
    }
}

impl shards_api::Stream for Looped {
    fn try_clone(&self) -> std::io::Result<Box<dyn shards_api::Stream>> {
        Ok(Box::new(Looped(self.0.try_clone()?)))
    }

    fn handle(&self) -> shards_api::Handle<'_> {
        #[cfg(unix)]
        return std::os::fd::AsFd::as_fd(&self.0);
        #[cfg(windows)]
        std::os::windows::io::AsSocket::as_socket(&self.0)
    }

    fn set_nonblocking(&self, on: bool) -> std::io::Result<()> {
        self.0.set_nonblocking(on)
    }

    fn shutdown(&self, how: Shutdown) -> std::io::Result<()> {
        self.0.shutdown(how)
    }

    fn set_read_timeout(&self, timeout: Option<Duration>) -> std::io::Result<()> {
        self.0.set_read_timeout(timeout)
    }

    fn set_write_timeout(&self, timeout: Option<Duration>) -> std::io::Result<()> {
        self.0.set_write_timeout(timeout)
    }

    /// Not asked of a guest's stream by Hex: refused, as loopback's are std's to keep.
    fn buffer_sizes(&self) -> std::io::Result<(usize, usize)> {
        Err(std::io::Error::new(
            std::io::ErrorKind::Unsupported,
            "a test's loopback stream reports no buffer sizes",
        ))
    }
}

/// A guest that records the patience each dial is given and answers every one on loopback.
struct Patient {
    listener: TcpListener,
    given: std::sync::Mutex<Vec<Duration>>,
    absolute: std::sync::Mutex<Vec<Instant>>,
}

impl shards_api::Guest for Patient {
    fn process_exited(&self) -> Result<bool, shards_api::Refusal> {
        Ok(false)
    }

    fn connect(
        &self,
        _port: u32,
        patience: Duration,
    ) -> Result<Box<dyn shards_api::Stream>, shards_api::Refusal> {
        self.given.lock().unwrap().push(patience);
        let conn = TcpStream::connect(self.listener.local_addr().unwrap()).unwrap();
        Ok(Box::new(Looped(conn)))
    }

    fn connect_before(
        &self,
        port: u32,
        at: std::time::Instant,
    ) -> Result<Box<dyn shards_api::Stream>, shards_api::Refusal> {
        self.absolute.lock().unwrap().push(at);
        let left = at
            .checked_duration_since(std::time::Instant::now())
            .filter(|left| !left.is_zero())
            .ok_or(shards_api::Refusal::Connect {
                port,
                failure: shards_api::HandshakeError::Expired {
                    phase: shards_api::HandshakePhase::Dial,
                },
            })?;
        let stream = self.connect(port, left)?;
        if std::time::Instant::now() >= at {
            return Err(shards_api::Refusal::Connect {
                port,
                failure: shards_api::HandshakeError::Expired {
                    phase: shards_api::HandshakePhase::ClearTimeouts,
                },
            });
        }
        Ok(stream)
    }

    fn stop(&self) -> Result<(), shards_api::Refusal> {
        Ok(())
    }

    fn wait_for(&self, _: Duration) -> Result<Option<shards_api::End>, shards_api::Refusal> {
        Ok(None)
    }

    fn identity(&self) -> Result<shards_api::Identity, shards_api::Refusal> {
        Err(shards_api::Refusal::Unsupported {
            what: "identity",
            why: "a loopback stand-in names no VM".into(),
        })
    }
}

/// A guest whose relay answers the first dial and is then gone. Its `connect_before` stands for the
/// retrying form, which would ask again until its deadline: so it answers only once, and is then expired.
/// Its single attempt is closed unanswered, as the VMM's muxer closes a CONNECT nobody listens for.
struct GoneRelay {
    listener: TcpListener,
    before: std::sync::Mutex<Vec<u32>>,
    once: std::sync::Mutex<Vec<u32>>,
}

impl shards_api::Guest for GoneRelay {
    fn process_exited(&self) -> Result<bool, shards_api::Refusal> {
        Ok(false)
    }

    fn connect(
        &self,
        port: u32,
        _: Duration,
    ) -> Result<Box<dyn shards_api::Stream>, shards_api::Refusal> {
        panic!("a link dials port {port} only within a deadline")
    }

    fn connect_before(
        &self,
        port: u32,
        _: std::time::Instant,
    ) -> Result<Box<dyn shards_api::Stream>, shards_api::Refusal> {
        let mut before = self.before.lock().unwrap();
        before.push(port);
        if before.len() > 1 {
            return Err(shards_api::Refusal::Connect {
                port,
                failure: shards_api::HandshakeError::Expired {
                    phase: shards_api::HandshakePhase::Dial,
                },
            });
        }
        let conn = TcpStream::connect(self.listener.local_addr().unwrap()).unwrap();
        Ok(Box::new(Looped(conn)))
    }

    fn connect_once_before(
        &self,
        port: u32,
        _: std::time::Instant,
    ) -> Result<Box<dyn shards_api::Stream>, shards_api::Refusal> {
        self.once.lock().unwrap().push(port);
        Err(shards_api::Refusal::Connect {
            port,
            failure: shards_api::HandshakeError::Closed,
        })
    }

    fn stop(&self) -> Result<(), shards_api::Refusal> {
        Ok(())
    }

    fn wait_for(&self, _: Duration) -> Result<Option<shards_api::End>, shards_api::Refusal> {
        Ok(None)
    }

    fn identity(&self) -> Result<shards_api::Identity, shards_api::Refusal> {
        Err(shards_api::Refusal::Unsupported {
            what: "identity",
            why: "a loopback stand-in names no VM".into(),
        })
    }
}

/// AC-T7.4.1: dial a guest whose relay answered the first dial and is then gone; expect the first dial
/// through `connect_before`, which waits for the relay to listen, and each later dial one
/// `connect_once_before` attempt, refused at once as closed unanswered, by type. The relay binds every
/// port before it serves any, so a later port that refuses has no relay behind it.
#[test]
fn a_later_dial_to_a_gone_relay_is_one_attempt_refused_at_once() {
    let guest = Arc::new(GoneRelay {
        listener: TcpListener::bind("127.0.0.1:0").unwrap(),
        before: std::sync::Mutex::new(Vec::new()),
        once: std::sync::Mutex::new(Vec::new()),
    });
    let link = shards::ShardsLink::new(guest.clone(), Duration::from_secs(60));
    link.dial(stream::CONTROL_PORT).unwrap();
    for port in [stream::STDIO_PORT, stream::CONTROL_PORT] {
        let refused = link.dial(port).err().unwrap();
        assert!(
            matches!(
                refused,
                hex_api::Refusal::Connect {
                    port: p,
                    failure: hex_api::ConnectFailure::Closed,
                } if p == port
            ),
            "{refused:?}"
        );
    }
    assert_eq!(*guest.before.lock().unwrap(), [stream::CONTROL_PORT]);
    assert_eq!(
        *guest.once.lock().unwrap(),
        [stream::STDIO_PORT, stream::CONTROL_PORT]
    );
}

/// HEX-GUEST-33, AC-T7.4.1 (§2, CLAUDE.md §2): dial a booted guest three times with no caller deadline, the
/// later two after its relay listens: expect each dial to reach Shards as `connect_before` with one finite
/// absolute deadline, the link's bound from that dial's start; never a relative patience, as Shards' client
/// awaits a zero patience's CONNECT answer without a bound.
#[test]
fn every_dial_carries_a_finite_deadline_after_the_first_listen() {
    let guest = Arc::new(Patient {
        listener: TcpListener::bind("127.0.0.1:0").unwrap(),
        given: std::sync::Mutex::new(Vec::new()),
        absolute: std::sync::Mutex::new(Vec::new()),
    });
    let bound = Duration::from_millis(354);
    let link = shards::ShardsLink::new(guest.clone(), bound);
    let mut windows = Vec::new();
    for port in [stream::CONTROL_PORT, stream::STDIO_PORT, stream::STDIO_PORT] {
        let began = Instant::now();
        link.dial(port).unwrap();
        windows.push((began + bound, Instant::now() + bound));
    }
    let absolute = guest.absolute.lock().unwrap().clone();
    let given = guest.given.lock().unwrap().clone();
    eprintln!("hex_guest_33_dials bound={bound:?} absolute={absolute:?} given={given:?}");
    assert_eq!(
        absolute.len(),
        windows.len(),
        "dials reached Shards with relative patience {given:?}, not a deadline (zero: an unbounded CONNECT)"
    );
    for (at, (earliest, latest)) in absolute.iter().zip(windows) {
        assert!(
            earliest <= *at && *at <= latest,
            "deadline {at:?} is not the bound from its dial's start, [{earliest:?}, {latest:?}]"
        );
    }
}

/// HEX-GUEST-33, AC-T7.4.1 (CLAUDE.md §2): connect with no caller deadline, plainly and in the event form, through a
/// Shards link whose guest takes the control connection and holds its relay's token until the link's bound has
/// surely passed, then sends it and answers the hello: expect each connect refused by type at the link's own setup
/// bound, the token's read with the dial, never connected late, as a relay that stays silent would hold it forever.
#[test]
fn a_relay_token_held_past_the_links_bound_refuses_the_connect() {
    let mut seen = Vec::new();
    for evented in [false, true] {
        let guest = Arc::new(Patient {
            listener: TcpListener::bind("127.0.0.1:0").unwrap(),
            given: std::sync::Mutex::new(Vec::new()),
            absolute: std::sync::Mutex::new(Vec::new()),
        });
        let listener = guest.listener.try_clone().unwrap();
        let bound = CONNECT_CAP;
        let relay = std::thread::spawn(move || {
            let (mut conn, _) = listener.accept().unwrap();
            // The connect's setup began before its dial, so its deadline is at most the bound past this accept:
            // hold the token until then, unless the connect gives up first and closes.
            let past = Instant::now().checked_add(bound).unwrap();
            loop {
                let left = past.saturating_duration_since(Instant::now());
                if left.is_zero() {
                    break;
                }
                conn.set_read_timeout(Some(left)).unwrap();
                match conn.peek(&mut [0u8; 1]) {
                    Ok(0) => return false,
                    Ok(_) => panic!("the client wrote before the relay's token"),
                    Err(e) => assert!(
                        matches!(
                            e.kind(),
                            std::io::ErrorKind::WouldBlock
                                | std::io::ErrorKind::TimedOut
                                | std::io::ErrorKind::Interrupted
                        ),
                        "{e}"
                    ),
                }
            }
            conn.set_read_timeout(None).unwrap();
            if conn.write_all(&TOKEN.to_le_bytes()).is_err() {
                return false;
            }
            // A connect that read the late token says hello: answer it, then hold until it goes.
            if !matches!(conn.peek(&mut [0u8; 1]), Ok(n) if n > 0) {
                return false;
            }
            let (h, _) = next_request(&mut conn);
            send_answer(&mut conn, h.id, &Answer::Hello(agents_hello()));
            let _ = conn.read(&mut [0u8; 1]);
            true
        });
        let link = own_link(shards::ShardsLink::new(guest.clone(), bound)).unwrap();
        let connected = if evented {
            Client::connect_evented(link, Arc::new(|| {})).map(drop)
        } else {
            Client::connect(link).map(drop)
        };
        let late = relay.join().unwrap();
        eprintln!(
            "hex_guest_33_token evented={evented} bound={bound:?} connected={connected:?} late={late}"
        );
        seen.push((evented, connected, late));
    }
    for (evented, connected, late) in seen {
        assert!(
            matches!(&connected, Err(Refusal::Os { op, why }) if op.contains("token") && why.contains("setup bound")),
            "evented={evented}: {connected:?}"
        );
        assert!(
            !late,
            "evented={evented}: connected on a token past its bound"
        );
    }
}

/// HEX-GUEST-33, AC-T7.4.1 (CLAUDE.md §2, §3): give a link a bound that names no finite deadline, zero (a boot
/// that spent its bound) or `Duration::MAX`, and dial: expect each refused by type before Shards is asked, as
/// Shards' client awaits either's CONNECT answer without a bound.
#[test]
fn a_link_bound_with_no_finite_deadline_is_refused_before_shards() {
    for bound in [Duration::ZERO, Duration::MAX] {
        let guest = Arc::new(Patient {
            listener: TcpListener::bind("127.0.0.1:0").unwrap(),
            given: std::sync::Mutex::new(Vec::new()),
            absolute: std::sync::Mutex::new(Vec::new()),
        });
        let link = shards::ShardsLink::new(guest.clone(), bound);
        let dialed = link.dial(stream::CONTROL_PORT).map(drop);
        let given = guest.given.lock().unwrap().clone();
        let absolute = guest.absolute.lock().unwrap().clone();
        eprintln!(
            "hex_guest_33_unbounded bound={bound:?} dialed={dialed:?} given={given:?} absolute={absolute:?}"
        );
        assert!(
            given.is_empty() && absolute.is_empty(),
            "bound {bound:?} reached Shards: patience {given:?}, deadlines {absolute:?}"
        );
        assert!(
            matches!(
                dialed,
                Err(Refusal::Invalid {
                    what: "guest dial bound",
                    ..
                })
            ),
            "{dialed:?}"
        );
    }
}

/// HEX-GUEST-33, AC-T7.4.1 (CLAUDE.md §2, §3): through Shards' own client, a guest whose muxer answers the first
/// CONNECT, then takes the next and never answers it: expect that dial refused by type at its deadline, the link's
/// bound from its start, not held for as long as the guest is silent. A child `cat` stands in for the guest's VM
/// process, which Shards holds by its pidfd and kills as the guest drops. The bound is the caller's own deadline for the
/// dial, the case's input; the dial's answer is waited for while the thread that dials lives (decision 9), never for a
/// window of the test's own, and the muxer is released once it has come, as Shards'
/// `withheld_reply_expires_before_fixture_release` releases its fixture.
#[cfg(target_os = "linux")]
#[test]
fn a_dial_the_guest_takes_and_never_answers_is_refused_at_its_deadline() {
    use shards_api::Guests;
    use std::io::{BufRead, BufReader};
    use std::os::unix::net::{UnixListener, UnixStream};
    use std::process::{Command, Stdio};

    let dir = tempfile::tempdir().unwrap();
    let vsock = dir.path().join("v");
    let listener = UnixListener::bind(&vsock).unwrap();
    let mut vm = Command::new("cat")
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .spawn()
        .unwrap();
    // proc(5): starttime is field 22, the 20th after the command's closing parenthesis.
    let stat = std::fs::read(format!("/proc/{}/stat", vm.id())).unwrap();
    let after = stat.iter().rposition(|b| *b == b')').unwrap() + 1;
    let started: u64 = std::str::from_utf8(&stat[after..])
        .unwrap()
        .split_ascii_whitespace()
        .nth(19)
        .unwrap()
        .parse()
        .unwrap();
    let spec = shards_api::GuestSpec {
        kernel: "kernel".into(),
        init: shards_api::Init::Executable("init".into()),
        cmdline: String::new(),
        vcpus: 1,
        gpu: false,
        gpu_devices: Vec::new(),
        memory_mib: shards_api::min_memory_mib(std::env::consts::ARCH),
        disks: vec![],
        shares: vec![],
        ports: vec![stream::CONTROL_PORT, stream::STDIO_PORT],
        binary: None,
    };
    let guest = shards_api::Shards::new()
        .attach(
            &shards_api::Identity {
                pid: vm.id(),
                started,
                vsock,
            },
            &spec,
        )
        .unwrap();
    let (release, held) = std::sync::mpsc::channel::<()>();
    let muxer = std::thread::spawn(move || {
        let ask = |conn: &UnixStream| {
            let mut line = String::new();
            BufReader::new(conn).read_line(&mut line).unwrap();
            line
        };
        let (mut answered, _) = listener.accept().unwrap();
        let first = ask(&answered);
        answered.write_all(b"OK 1\n").unwrap();
        let (silent, _) = listener.accept().unwrap();
        let second = ask(&silent);
        // Released, both close: a dial still waiting on `silent` then reads its end.
        let _ = held.recv();
        (first, second)
    });
    // The caller's own deadline for its first dial (`ShardsLink::new`'s bound), the case's input: whatever it is, a dial
    // the guest takes and never answers is refused at it, and not before. Short, so the case is.
    let bound = Duration::from_millis(200);
    let link = shards::ShardsLink::new(Arc::from(guest), bound);
    let control = link.dial(stream::CONTROL_PORT).unwrap();
    let (sent, answer) = std::sync::mpsc::channel();
    let began = Instant::now();
    let dialing = std::thread::spawn(move || {
        let dialed = link.dial(stream::STDIO_PORT).map(drop);
        let _ = sent.send((dialed, Instant::now()));
        link
    });
    // The dial's answer, waited for while its thread lives, never for a window of the test's own (decision 9): a dial
    // held for as long as the guest is silent holds its thread, which the muxer frees only once the answer has come.
    static DIALED: hex_core::patience::Patience =
        hex_core::patience::Patience::measuring("test.hex_client.guest.silent_dial_answered");
    let got = hex_testkit::waits::follow::within(
        &DIALED,
        "the silent dial's answer",
        || {
            if dialing.is_finished() {
                hex_testkit::waits::follow::Party::Ended
            } else {
                hex_testkit::waits::follow::Party::Live
            }
        },
        |by| match by {
            None => answer.recv().ok(),
            Some(at) => answer
                .recv_timeout(at.saturating_duration_since(Instant::now()))
                .ok(),
        },
    );
    let _ = release.send(());
    let link = dialing.join().unwrap();
    let (first, second) = muxer.join().unwrap();
    // Shards kills the stand-in by its pidfd as the guest drops; reaping it is this test's, its parent's.
    drop((control, link));
    let reaped = vm.wait();
    eprintln!(
        "hex_guest_33_silent bound={bound:?} began={began:?} got={got:?} first={first:?} second={second:?} reaped={reaped:?}"
    );
    assert_eq!(first, format!("CONNECT {}\n", stream::CONTROL_PORT));
    assert_eq!(second, format!("CONNECT {}\n", stream::STDIO_PORT));
    let Ok((dialed, at)) = got else {
        panic!("{got:?}: the dial's thread ended with no answer of its dial's");
    };
    assert!(dialed.is_err(), "a silent guest's dial succeeded");
    assert!(
        at >= began + bound,
        "refused at {at:?}, before its deadline {:?}",
        began + bound
    );
}

/// HEX-RT-17, AC-T7.4.1 (CLAUDE.md §2): open an offered guest exec's stdin and stdout as a Windows caller does,
/// through Shards' borrowed connector, against a stand-in VM process and its vsock muxer. One muxer answers each
/// CONNECT with OK a moment after reading it, under a deadline the test never reaches; another never answers, under
/// a near one. Expect the first opened, each stream named for the exec and the caller's ends blocking; the second
/// refused by type in its CONNECT's read phase, never left waiting.
#[cfg(target_os = "linux")]
#[test]
fn an_offered_streams_connect_waits_for_its_ok_within_the_calls_deadline() {
    use std::os::unix::net::{UnixListener, UnixStream};
    use std::process::{Command, Stdio};

    // Each connection's CONNECT line and, once it is answered, its name; the answered ones, held open.
    fn muxer(
        listener: UnixListener,
        answer: Option<Duration>,
    ) -> std::thread::JoinHandle<(Vec<Vec<u8>>, Vec<UnixStream>)> {
        std::thread::spawn(move || {
            let (mut heard, mut held) = (Vec::new(), Vec::new());
            for _ in 0..2 {
                let (mut conn, _) = listener.accept().unwrap();
                let mut line = Vec::new();
                while line.last() != Some(&b'\n') {
                    let mut byte = [0u8; 1];
                    conn.read_exact(&mut byte).unwrap();
                    line.push(byte[0]);
                }
                let Some(after) = answer else {
                    // Silent: held until the client gives up and closes.
                    let _ = conn.read(&mut [0u8; 1]);
                    heard.push(line);
                    return (heard, held);
                };
                // The OK only once `after` has passed, unless the client gives up and closes first.
                let at = Instant::now().checked_add(after).unwrap();
                loop {
                    let left = at.saturating_duration_since(Instant::now());
                    if left.is_zero() {
                        break;
                    }
                    conn.set_read_timeout(Some(left)).unwrap();
                    match rustix::net::recv(&conn, &mut [0u8; 1], rustix::net::RecvFlags::PEEK) {
                        Ok((0, _)) => {
                            heard.push(line);
                            return (heard, held);
                        }
                        Ok(_) => panic!("the client wrote before its CONNECT's OK"),
                        Err(e) => assert!(
                            e == rustix::io::Errno::AGAIN || e == rustix::io::Errno::INTR,
                            "{e}"
                        ),
                    }
                }
                conn.set_read_timeout(None).unwrap();
                conn.write_all(b"OK 1\n").unwrap();
                let mut name = [0u8; stream::NAME_BYTES];
                conn.read_exact(&mut name).unwrap();
                line.extend_from_slice(&name);
                heard.push(line);
                held.push(conn);
            }
            (heard, held)
        })
    }

    let dir = tempfile::tempdir().unwrap();
    let mut vm = Command::new("cat")
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .spawn()
        .unwrap();
    // proc(5): starttime is field 22, the 20th after the command's closing parenthesis.
    let stat = std::fs::read(format!("/proc/{}/stat", vm.id())).unwrap();
    let after = stat.iter().rposition(|b| *b == b')').unwrap() + 1;
    let started: u64 = std::str::from_utf8(&stat[after..])
        .unwrap()
        .split_ascii_whitespace()
        .nth(19)
        .unwrap()
        .parse()
        .unwrap();
    let offer = |vsock: std::path::PathBuf| wire::StreamOffer {
        pid: vm.id(),
        started,
        vsock,
        token: TOKEN,
        exec: 9,
        streams: wire::Streams {
            stdin: true,
            stdout: true,
            stderr: false,
        },
    };
    let answering = dir.path().join("a");
    let answered = muxer(UnixListener::bind(&answering).unwrap(), Some(CONNECT_CAP));
    let far = ReplyDeadline::caller_before(Instant::now() + Duration::from_secs(30)).unwrap();
    let opened = open_offered(&offer(answering), Some(far));
    let (heard, held) = answered.join().unwrap();
    let silent_path = dir.path().join("s");
    let silent = muxer(UnixListener::bind(&silent_path).unwrap(), None);
    let near = ReplyDeadline::caller_before(Instant::now() + CONNECT_CAP).unwrap();
    let refused = open_offered(&offer(silent_path), Some(near));
    let (unanswered, _) = silent.join().unwrap();
    drop(vm.stdin.take());
    let reaped = vm.wait();
    let named = |stream| {
        let mut line = format!("CONNECT {}\n", stream::STDIO_PORT).into_bytes();
        line.extend_from_slice(&stream::name(Named {
            exec: Exec { conn: TOKEN, id: 9 },
            stream,
        }));
        line
    };
    let blocking = |end: &Option<hex_api::End>| {
        end.as_ref().is_some_and(|end| {
            !rustix::fs::fcntl_getfl(end)
                .unwrap()
                .contains(rustix::fs::OFlags::NONBLOCK)
        })
    };
    eprintln!(
        "hex_rt_17_offered opened={opened:?} heard={} refused={refused:?} unanswered={} reaped={reaped:?}",
        heard.len(),
        unanswered.len()
    );
    let given = opened.expect("an answered offer's streams, opened");
    assert!(blocking(&given.stdin) && blocking(&given.stdout) && given.stderr.is_none());
    assert_eq!(heard, [named(stream::Stdio::In), named(stream::Stdio::Out)]);
    assert!(
        matches!(
            refused,
            Err(Refusal::Connect {
                port: stream::STDIO_PORT,
                failure: hex_api::ConnectFailure::Expired {
                    phase: hex_api::ConnectPhase::Read
                } | hex_api::ConnectFailure::Io {
                    phase: hex_api::ConnectPhase::Read,
                    ..
                },
            })
        ),
        "{refused:?}"
    );
    assert_eq!(
        unanswered,
        [format!("CONNECT {}\n", stream::STDIO_PORT).into_bytes()]
    );
    drop(held);
}

/// HEX-RT-17, HEX-GUEST-41: ask an actual Shards simulator for stdin and stdout, then use their raw descriptors as
/// the caller's files. Expect blocking ends that echo to EOF, while the control connection remains non-blocking.
/// Shards waits around its non-blocking kernel sockets in its Stream wrapper; that wrapper is gone at handoff.
#[test]
fn asked_shards_streams_wait_as_the_callers_files_do() {
    use shards_api::Guests;

    let guests = shards_api::sim::SimGuests::new(|host| {
        host.listen(stream::CONTROL_PORT);
        host.listen(stream::STDIO_PORT);
        let (port, mut control) = host.accept().unwrap();
        assert_eq!(port, stream::CONTROL_PORT);
        control.write_all(&TOKEN.to_le_bytes()).unwrap();
        let mut named = std::collections::BTreeMap::new();
        for _ in 0..2 {
            let (port, mut conn) = host.accept().unwrap();
            assert_eq!(port, stream::STDIO_PORT);
            let mut name = [0u8; stream::NAME_BYTES];
            conn.read_exact(&mut name).unwrap();
            let named_stream = stream::read_name(&name).unwrap();
            assert_eq!(named_stream.exec, Exec { conn: TOKEN, id: 1 });
            named.insert(named_stream.stream, conn);
        }
        let mut stdin = named.remove(&stream::Stdio::In).unwrap();
        let mut stdout = named.remove(&stream::Stdio::Out).unwrap();
        std::io::copy(&mut stdin, &mut stdout).unwrap();
        shards_api::End::PowerOff
    });
    let guest = guests
        .boot(&shards_api::GuestSpec {
            kernel: "kernel".into(),
            init: shards_api::Init::Executable("init".into()),
            cmdline: String::new(),
            vcpus: 1,
            gpu: false,
            gpu_devices: Vec::new(),
            memory_mib: shards_api::min_memory_mib(std::env::consts::ARCH),
            disks: vec![],
            shares: vec![],
            ports: vec![stream::CONTROL_PORT, stream::STDIO_PORT],
            binary: None,
        })
        .unwrap();
    // A link's bound names a finite deadline (HEX-GUEST-33): the first dial's, as the node gives it before any boot
    // (`first_dial_bound`), the guest's recorded boot to ready, the boot and listen it waits on (decision 9).
    let bound = shards::recorded_boot().unwrap();
    let transport =
        Stream::dial(own_link(shards::ShardsLink::new(Arc::from(guest), bound)).unwrap()).unwrap();
    let given = transport
        .asks(
            1,
            hex_api::Ask {
                stdin: true,
                stdout: true,
                stderr: false,
            },
        )
        .unwrap();
    let (stdin, stdout) = (given.stdin.unwrap(), given.stdout.unwrap());
    #[cfg(unix)]
    for fd in [&stdin, &stdout] {
        assert!(
            !rustix::fs::fcntl_getfl(fd)
                .unwrap()
                .contains(rustix::fs::OFlags::NONBLOCK),
            "a raw given end must wait without Shards' Stream wrapper"
        );
    }
    #[cfg(unix)]
    assert!(
        rustix::fs::fcntl_getfl(transport.handle())
            .unwrap()
            .contains(rustix::fs::OFlags::NONBLOCK)
    );
    #[cfg(windows)]
    {
        assert!(stdin.socket().is_some());
        assert!(stdout.socket().is_some());
        // A retained transfer copy closing must not shut down the caller's socket stream.
        let retained: hex_api::End = stdin.socket().unwrap().try_clone_to_owned().unwrap().into();
        drop(retained);
        let empty = hex_afunix::receive_socket(transport.handle(), &mut [0]).unwrap_err();
        assert_eq!(empty.kind(), std::io::ErrorKind::WouldBlock);
    }
    let mut stdin = hex_api::OwnedStream::from(stdin);
    stdin.write_all(b"round trip\n").unwrap();
    drop(stdin);
    let mut output = String::new();
    hex_api::OwnedStream::from(stdout)
        .read_to_string(&mut output)
        .unwrap();
    assert_eq!(output, "round trip\n");
}

/// Native plan step 8 (1), CLAUDE.md §2: the event form's writes queue behind a backlog bounded in bytes.
/// To a peer that never reads, frames go while the socket and the backlog have room; the one that wouldn't
/// fit is refused whole, `Exhausted` naming the backlog and its bound, nothing past the bound is ever
/// held, and the loop is told once, when bytes first wait.
#[test]
fn the_event_forms_writes_are_bounded_and_refused_whole_past_the_bound() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let mut writer = TcpStream::connect(listener.local_addr().unwrap()).unwrap();
    // Held, never read.
    let (_peer, _) = listener.accept().unwrap();
    writer.set_nonblocking(true).unwrap();
    let told = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let telling = Arc::clone(&told);
    let mut backlog = Backlog {
        bytes: VecDeque::new(),
        bound: 4096,
        wants_room: Arc::new(move || {
            telling.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        }),
    };
    let frame = vec![7u8; 1000];
    let mut refused = None;
    // Bounded: 20 MB, past any loopback socket's buffers, so the backlog fills well before it ends.
    for _ in 0..20_000 {
        match backlog.send(&mut writer, &frame) {
            Ok(()) => assert!(backlog.bytes.len() <= 4096, "{} held", backlog.bytes.len()),
            Err(Sent::Refused(why)) => {
                refused = Some(why);
                break;
            }
            Err(Sent::Gone | Sent::Full(_)) => panic!("the peer went, or the send waited"),
        }
    }
    assert_eq!(
        refused,
        Some(Refusal::Exhausted {
            what: BACKLOG,
            bound: 4096
        })
    );
    assert!(backlog.bytes.len() <= 4096);
    assert_eq!(told.load(std::sync::atomic::Ordering::SeqCst), 1);
}

/// Native plan step 8 (1): the event form's pump, its connection ended by the agent, says `Ended`, and
/// every call waiting through it hears why, as a typed refusal; a call begun after is refused at once,
/// never left waiting.
#[cfg(unix)]
#[test]
fn a_pumps_end_reaches_every_call_waiting_through_it() {
    let (control, _stdio, link) = listeners();
    let agent = std::thread::spawn(move || {
        let (mut conn, _) = control.accept().unwrap();
        hello(&mut conn);
        // Reads the inspect, and closes without answering it.
        let _ = next_request(&mut conn);
    });
    let (client, mut pump) = Client::connect_evented(link, Arc::new(|| {})).unwrap();
    let (tx, rx) = std::sync::mpsc::channel();
    client
        .begin(
            Call::Inspect(BoxId::new("b").unwrap()),
            Stdio::null(),
            Box::new(move |_, got| tx.send(got).unwrap()),
            Box::new(|_| {}),
        )
        .unwrap();
    agent.join().unwrap();
    let deadline = std::time::Instant::now() + Duration::from_secs(30);
    loop {
        if pump.ready() == Pumped::Ended {
            break;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "the pump never saw its end"
        );
        let handle = pump.handle();
        let mut fds = [rustix::event::PollFd::new(
            &handle,
            rustix::event::PollFlags::IN,
        )];
        let wait = rustix::event::Timespec {
            tv_sec: 1,
            tv_nsec: 0,
        };
        let _ = rustix::event::poll(&mut fds, Some(&wait));
    }
    let got = rx.recv_timeout(Duration::from_secs(30)).unwrap();
    assert!(
        matches!(&got, Err(Refusal::Broken(why)) if why.contains("closed")),
        "{got:?}"
    );
    assert!(
        client
            .begin(
                Call::Inspect(BoxId::new("c").unwrap()),
                Stdio::null(),
                Box::new(|_, _| {}),
                Box::new(|_| {}),
            )
            .is_err()
    );
}

/// Native plan step 8, guest stdio (HEX-GUEST-41): the event form, a node's, copies no stream: an exec begun with
/// a pipe of the caller's own is refused as invalid stdio and nothing goes to the guest, so the guest's next
/// request is the inspect begun after it.
#[test]
fn the_event_form_takes_no_pipe_of_the_callers_own() {
    let (control, _stdio, link) = listeners();
    let agent = std::thread::spawn(move || {
        let (mut conn, _) = control.accept().unwrap();
        hello(&mut conn);
        let (_, request) = next_request(&mut conn);
        request
    });
    let (client, _pump) = Client::connect_evented(link, Arc::new(|| {})).unwrap();
    let (r, _w) = std::io::pipe().unwrap();
    let carried = client.begin(
        Call::Exec {
            id: BoxId::new("b").unwrap(),
            program: Program::of(&["cat"]),
            streams: wire::Streams::default(),
        },
        Stdio {
            stdin: Some(r),
            ..Stdio::null()
        },
        Box::new(|_, _| {}),
        Box::new(|_| {}),
    );
    assert!(
        matches!(&carried, Err(Refusal::Invalid { what: "stdio", .. })),
        "{carried:?}"
    );
    client
        .begin(
            Call::Inspect(BoxId::new("b").unwrap()),
            Stdio::null(),
            Box::new(|_, _| {}),
            Box::new(|_| {}),
        )
        .unwrap();
    assert_eq!(
        agent.join().unwrap(),
        Request::Call(Call::Inspect(BoxId::new("b").unwrap()))
    );
}

/// AC-T7.4.1: permanent selector mistakes and temporary storage exhaustion retain distinct
/// public categories; the adapter never turns either into an opaque operating-system retry.
#[test]
fn physical_gpu_refusals_preserve_retry_category() {
    for cause in [
        shards_api::PhysicalGpuError::Syntax,
        shards_api::PhysicalGpuError::Duplicate,
    ] {
        assert!(matches!(
            shards::refusal("booting", &shards_api::Refusal::Gpu(cause)),
            Refusal::Invalid {
                what: "physical GPU selection",
                ..
            }
        ));
    }
    assert!(
        matches!(shards::refusal("booting", &shards_api::Refusal::Gpu(shards_api::PhysicalGpuError::Allocation)),
        Refusal::Exhausted { what: "physical GPU selection storage", bound } if bound == shards_api::physical::MAX_PHYSICAL_GPUS)
    );
}

/// AC-T7.4.1 (Shards 18f59d497, D232): a VM process that refuses its GPU selection says so in place of a receipt,
/// certain, which is the selection's own refusal; a report writer closed with nothing written is the VM process ended
/// before its renderer said anything of its selection, which leaves the selection's fate unknown. Do: map each kind of
/// the VM process's refusal, and an unwritten report. Expect: each refusal Invalid, naming the selection, with its
/// reason's words kept; the unwritten report an OS refusal of the call that says so, never Invalid.
#[test]
fn a_vm_process_refusing_its_gpu_selection_says_why() {
    use shards_api::GpuProbeError;
    use shards_api::physical::{GpuRefusal, JoinError};
    for (refused, says) in [
        (
            GpuRefusal::Join(JoinError::Missing),
            "no exact live Vulkan identity",
        ),
        (GpuRefusal::NoHostMemory, "no memory the host can map"),
        (GpuRefusal::Acknowledgment(7), "ACK (7)"),
    ] {
        let mapped = shards::refusal(
            "booting",
            &shards_api::Refusal::GpuProbe(GpuProbeError::Refused(refused)),
        );
        assert!(
            matches!(&mapped, Refusal::Invalid { what: "physical GPU selection", why } if why.contains(says)),
            "{mapped:?}"
        );
    }
    let unreported = shards::refusal(
        "booting",
        &shards_api::Refusal::GpuProbe(GpuProbeError::Unreported),
    );
    assert!(
        matches!(&unreported, Refusal::Os { op: "booting", why } if why.contains("no report")),
        "{unreported:?}"
    );
}

/// AC-T7.4.1 (Shards R04): pass Shards' CONNECT failure in every phase and cause through the
/// adapter, an owned guest's (`Connect`) and a borrowed one's (`Borrowed`'s channel, as a Windows
/// caller opens an offered stream): expect Hex's typed refusal with the same port, phase, cause and
/// raw code, a kind Hex has no tag for unnamed (never `Other`), and Shards' text-only channel
/// refusal still an OS refusal.
#[test]
fn shards_connect_failures_keep_port_phase_cause_and_code() {
    use hex_api::{ConnectFailure, ConnectPhase, ConnectReply, IoKind};
    use shards_api::{HandshakeError, HandshakePhase, ReplyError};
    let untagged = (1..=200)
        .map(|code| io::Error::from_raw_os_error(code).kind())
        .find(|kind| !IoKind::NAMED.contains(kind))
        .unwrap();
    let mut cases = vec![
        (HandshakeError::Closed, ConnectFailure::Closed),
        (
            HandshakeError::Reply(ReplyError::TooLong),
            ConnectFailure::Reply(ConnectReply::TooLong),
        ),
        (
            HandshakeError::Reply(ReplyError::NotOk),
            ConnectFailure::Reply(ConnectReply::NotOk),
        ),
    ];
    for (sdk, hex) in [
        (HandshakePhase::Dial, ConnectPhase::Dial),
        (HandshakePhase::Write, ConnectPhase::Write),
        (HandshakePhase::Read, ConnectPhase::Read),
        (HandshakePhase::ClearTimeouts, ConnectPhase::ClearTimeouts),
    ] {
        cases.push((
            HandshakeError::Expired { phase: sdk },
            ConnectFailure::Expired { phase: hex },
        ));
        cases.push((
            HandshakeError::Io {
                phase: sdk,
                kind: io::ErrorKind::Other,
                code: None,
            },
            ConnectFailure::Io {
                phase: hex,
                kind: IoKind::new(io::ErrorKind::Other),
                code: None,
            },
        ));
        cases.push((
            HandshakeError::Io {
                phase: sdk,
                kind: untagged,
                code: Some(-7),
            },
            ConnectFailure::Io {
                phase: hex,
                kind: IoKind::UNNAMED,
                code: Some(-7),
            },
        ));
    }
    for (failure, expected) in cases {
        let borrowed = shards_api::Refusal::Borrowed {
            port: 78,
            failure: shards_api::BorrowFailure::Channel(failure.clone()),
        };
        let connect = shards_api::Refusal::Connect { port: 77, failure };
        assert_eq!(
            shards::refusal("connecting to the guest", &connect),
            Refusal::Connect {
                port: 77,
                failure: expected,
            },
            "{connect:?}"
        );
        assert_eq!(
            shards::refusal("opening an offered guest stream", &borrowed),
            Refusal::Connect {
                port: 78,
                failure: expected,
            },
            "{borrowed:?}"
        );
    }
    assert_ne!(IoKind::new(untagged), IoKind::new(io::ErrorKind::Other));
    let channel = shards_api::Refusal::Channel {
        port: 77,
        why: "closed".into(),
    };
    assert!(matches!(
        shards::refusal("connecting to the guest", &channel),
        Refusal::Os {
            op: "connecting to the guest",
            ..
        }
    ));
}

// Caller policies for these stand-ins, not production waits or calibration inputs.
const CONNECT_CAP: Duration = Duration::from_millis(25);
const CONNECT_OBSERVE: Duration = Duration::from_millis(100);

/// A connect's answer, the next message from `from`, following `connecting`, the thread that sends it, by its life
/// (decision 9): the wait goes on while that thread lives, and fails only once it has ended with nothing sent, never at
/// a window of the test's own.
fn connected_by<T>(
    wait: &hex_core::patience::Patience,
    from: &std::sync::mpsc::Receiver<T>,
    connecting: &std::thread::JoinHandle<()>,
) -> Result<T, hex_testkit::waits::follow::Unmet> {
    use hex_testkit::waits::follow::{self, Party};
    follow::within(
        wait,
        "the connect's answer",
        || {
            if connecting.is_finished() {
                Party::Ended
            } else {
                Party::Live
            }
        },
        |by| match by {
            None => from.recv().ok(),
            Some(at) => from
                .recv_timeout(at.saturating_duration_since(Instant::now()))
                .ok(),
        },
    )
}

#[derive(Clone, Copy, Debug)]
enum ConnectForm {
    Ordinary,
    Evented,
}

fn connect_form(
    form: ConnectForm,
    link: LinkOwner,
    at: Instant,
) -> Result<(Client, Option<Pump>), Refusal> {
    match form {
        ConnectForm::Ordinary => Client::connect_before(link, at).map(|client| (client, None)),
        ConnectForm::Evented => Client::connect_evented_before(link, Arc::new(|| {}), at)
            .map(|(client, pump)| (client, Some(pump))),
    }
}

#[derive(Default)]
struct ConnectTrace {
    dials: Mutex<Vec<(u32, Option<Instant>)>>,
    read: std::sync::atomic::AtomicUsize,
    written: Mutex<Vec<u8>>,
    native_clones: std::sync::atomic::AtomicUsize,
    facade_drops: std::sync::atomic::AtomicUsize,
}

struct ConnectLink {
    socket: Mutex<Option<TcpStream>>,
    budget: Option<Duration>,
    trace: Arc<ConnectTrace>,
}

impl Link for ConnectLink {
    fn reply_budget(&self) -> Option<Duration> {
        self.budget
    }

    fn dial(&self, port: u32) -> Result<DuplexOwner, Refusal> {
        self.dial_before(port, None)
    }

    fn dial_before(&self, port: u32, at: Option<Instant>) -> Result<DuplexOwner, Refusal> {
        self.trace.dials.lock().unwrap().push((port, at));
        let socket = self
            .socket
            .lock()
            .unwrap()
            .take()
            .ok_or_else(|| Refusal::Broken("constructor probe has no connection".into()))?;
        own_duplex(ConnectSocket {
            socket,
            trace: Arc::clone(&self.trace),
        })
    }
}

struct ConnectSocket {
    socket: TcpStream,
    trace: Arc<ConnectTrace>,
}

impl Drop for ConnectSocket {
    fn drop(&mut self) {
        self.trace
            .facade_drops
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
    }
}

impl Read for ConnectSocket {
    fn read(&mut self, bytes: &mut [u8]) -> io::Result<usize> {
        let n = self.socket.read(bytes)?;
        self.trace
            .read
            .fetch_add(n, std::sync::atomic::Ordering::SeqCst);
        Ok(n)
    }
}

impl Write for ConnectSocket {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        let n = self.socket.write(bytes)?;
        self.trace
            .written
            .lock()
            .unwrap()
            .extend_from_slice(&bytes[..n]);
        Ok(n)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.socket.flush()
    }
}

impl Duplex for ConnectSocket {
    fn try_clone(&self) -> io::Result<DuplexOwner> {
        self.trace
            .native_clones
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        own_duplex(Self {
            socket: self.socket.try_clone()?,
            trace: Arc::clone(&self.trace),
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
        Duplex::handle(&self.socket)
    }

    fn set_nonblocking(&self, on: bool) -> io::Result<()> {
        self.socket.set_nonblocking(on)
    }
}

fn connect_link(
    socket: Option<TcpStream>,
    budget: Option<Duration>,
) -> (LinkOwner, Arc<ConnectTrace>) {
    let trace = Arc::new(ConnectTrace::default());
    let link = own_link(ConnectLink {
        socket: Mutex::new(socket),
        budget,
        trace: Arc::clone(&trace),
    })
    .unwrap();
    (link, trace)
}

fn connect_pair() -> (TcpStream, TcpStream) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let client =
        TcpStream::connect_timeout(&listener.local_addr().unwrap(), CONNECT_OBSERVE).unwrap();
    // The connect has succeeded, so this accept waits only for the kernel to queue that
    // connection; a nonblocking accept could run before it (macOS: WouldBlock, os error 35).
    let (peer, _) = listener.accept().unwrap();
    (client, peer)
}

#[derive(Debug)]
struct ConstructorHello {
    header: Option<Header>,
    written: Vec<u8>,
    pending: bool,
    buffered: usize,
}

fn constructor_hello(trace: &ConnectTrace) -> ConstructorHello {
    let written = trace.written.lock().unwrap();
    assert!(written.len() <= stream::MAX_FRAME);
    let mut decoder = Deframer::new(wire::MAX_MESSAGE);
    let mut consumed = 0;
    let mut header = None;
    // take() first consumes the length prefix, then the body. Every nonempty
    // step must make progress, so captured bytes also bound the step count.
    for _ in 0..written.len() {
        if consumed == written.len() {
            break;
        }
        let (took, message) = decoder.take(&written[consumed..]).unwrap();
        assert!(took > 0 && took <= written.len() - consumed);
        consumed += took;
        if let Some(message) = message {
            assert!(
                header.is_none(),
                "constructor emitted more than one message"
            );
            assert_eq!(consumed, written.len(), "bytes after the complete Hello");
            let (got, request) = wire::read_request_apart(&message, wire::MAX_MESSAGE).unwrap();
            assert!(matches!(request, Request::Hello { build } if build == wire::build()));
            header = Some(got);
        }
    }
    assert_eq!(consumed, written.len());
    ConstructorHello {
        header,
        written: written.clone(),
        pending: decoder.in_frame(),
        buffered: decoder.buffered(),
    }
}

/// AC-T7.4.1: both public constructors reject expired/invalid authority before dial,
/// preserve an exact absolute instant with no Link budget, and honor a smaller configured cap.
#[test]
fn absolute_connect_rejects_before_dial_and_preserves_smaller_caps() {
    for form in [ConnectForm::Ordinary, ConnectForm::Evented] {
        let (link, trace) = connect_link(None, None);
        let expired = connect_form(form, link, Instant::now()).map(drop);
        assert!(matches!(expired, Err(Refusal::Os { .. })), "{expired:?}");
        assert!(trace.dials.lock().unwrap().is_empty());

        for invalid in [Duration::ZERO, Duration::MAX] {
            let (link, trace) = connect_link(None, Some(invalid));
            let at = Instant::now().checked_add(CONNECT_OBSERVE).unwrap();
            let refused = connect_form(form, link, at).map(drop);
            assert!(
                matches!(
                    refused,
                    Err(Refusal::Invalid {
                        what: "client reply budget",
                        ..
                    })
                ),
                "{refused:?}"
            );
            assert!(trace.dials.lock().unwrap().is_empty());
        }

        let (link, trace) = connect_link(None, None);
        let at = Instant::now().checked_add(CONNECT_OBSERVE).unwrap();
        let refused = connect_form(form, link, at).map(drop);
        assert!(matches!(refused, Err(Refusal::Broken(_))));
        assert_eq!(
            *trace.dials.lock().unwrap(),
            [(stream::CONTROL_PORT, Some(at))]
        );

        let (link, trace) = connect_link(None, Some(CONNECT_CAP));
        let at = Instant::now().checked_add(CONNECT_OBSERVE).unwrap();
        let before = Instant::now();
        let refused = connect_form(form, link, at).map(drop);
        let after = Instant::now();
        assert!(matches!(refused, Err(Refusal::Broken(_))));
        let dials = trace.dials.lock().unwrap();
        assert_eq!(dials.len(), 1);
        let (port, selected) = dials[0];
        let selected = selected.unwrap();
        assert_eq!(port, stream::CONTROL_PORT);
        assert!(before.checked_add(CONNECT_CAP).unwrap() <= selected);
        assert!(selected <= after.checked_add(CONNECT_CAP).unwrap());
        assert!(selected < at);
    }
}

/// AC-T7.4.1: actual partial token reads and the hello send/wait consume the same
/// original cap even when Link has no default budget. Both constructor forms are covered.
#[test]
fn absolute_connect_caps_partial_token_and_unanswered_hello() {
    use std::sync::atomic::Ordering;
    use std::sync::mpsc;

    let mut observations = Vec::with_capacity(4);
    for form in [ConnectForm::Ordinary, ConnectForm::Evented] {
        for whole_token in [false, true] {
            let (socket, mut peer) = connect_pair();
            let token = TOKEN.to_le_bytes();
            let prefix = if whole_token { &token[..] } else { &token[..1] };
            peer.write_all(prefix).unwrap();
            let (link, trace) = connect_link(Some(socket), None);
            let at = Instant::now().checked_add(CONNECT_CAP).unwrap();
            let (finished, answer) = mpsc::sync_channel(1);
            let worker = std::thread::spawn(move || {
                let result = connect_form(form, link, at).map(drop);
                let _ = finished.send((Instant::now(), result));
            });
            // No assertion may bypass this owned socket close and actor join.
            static CAPPED: hex_core::patience::Patience =
                hex_core::patience::Patience::measuring("test.hex_client.guest.connect_capped");
            let observed = connected_by(&CAPPED, &answer, &worker);
            let stopped = peer.shutdown(Shutdown::Both);
            drop(peer);
            let joined = worker.join();
            let read = trace.read.load(Ordering::SeqCst);
            observations.push((
                form,
                whole_token,
                at,
                observed,
                stopped,
                joined.is_ok(),
                read,
                trace,
            ));
        }
    }
    let mut uncommitted = Vec::with_capacity(2);
    // All four owned peers are closed and actors joined before any assertion.
    for (form, whole_token, at, observed, stopped, joined, read, trace) in observations {
        let hello = constructor_hello(&trace);
        eprintln!(
            "client_connect_absolute form={form:?} whole_token={whole_token} at={at:?} observed={observed:?} read={read} stopped={stopped:?} joined={joined} hello={hello:?}"
        );
        assert!(joined);
        let (returned, result) = observed.expect("constructor did not return before owned cleanup");
        assert_eq!(
            read,
            if whole_token { stream::TOKEN_BYTES } else { 1 },
            "missing actual read progress is a harness miss"
        );
        assert!(matches!(result, Err(Refusal::Os { .. })), "{result:?}");
        assert_eq!(
            *trace.dials.lock().unwrap(),
            [(stream::CONTROL_PORT, Some(at))]
        );
        if whole_token {
            if let Some(header) = hello.header {
                assert_eq!(header.id, 1);
                assert!(!hello.pending && hello.buffered == 0);
                eprintln!(
                    "client_connect_hello form={form:?} phase=committed_hello withheld_answer_proof=true"
                );
            } else {
                let phase = if hello.written.is_empty() {
                    "learned_before_send"
                } else {
                    "learned_during_incomplete_send"
                };
                eprintln!(
                    "client_connect_hello form={form:?} phase={phase} withheld_answer_proof=false status=UNVERIFIED written={:02x?}",
                    hello.written
                );
                assert!(
                    matches!(&result, Err(Refusal::Os { op: "sending to the node agent", why }) if why.contains("twice")),
                    "uncommitted Hello must be explained by its smaller learned send bound: {result:?}"
                );
                uncommitted.push((form, phase));
            }
        } else {
            assert!(hello.written.is_empty());
            assert!(
                matches!(&result, Err(Refusal::Os { why, .. }) if why.contains("absolute"))
                    && returned >= at
            );
        }
    }
    assert!(
        uncommitted.is_empty(),
        "UNVERIFIED withheld-answer phase: no complete actual Hello; harness witness miss, not a production timeout failure: {uncommitted:?}"
    );
}

/// AC-T7.4.1: a complete real hello reaches both constructors; the event form keeps
/// the same Link owner in its existing Pump and stores no caller deadline as a lifetime cap.
#[test]
fn absolute_connect_completes_and_evented_retains_the_same_link() {
    use std::sync::mpsc;

    for form in [ConnectForm::Ordinary, ConnectForm::Evented] {
        let (socket, mut peer) = connect_pair();
        peer.write_all(&TOKEN.to_le_bytes()).unwrap();
        // The fresh Shared's first request is 1; independently decode its actual Hello below.
        send_answer(&mut peer, 1, &Answer::Hello(agents_hello()));
        let (link, trace) = connect_link(Some(socket), None);
        let same_link = triomphe::Arc::clone(&link);
        let at = Instant::now().checked_add(CONNECT_OBSERVE).unwrap();
        let (finished, answer) = mpsc::sync_channel(1);
        let worker = std::thread::spawn(move || {
            let result = connect_form(form, link, at).map(|(client, pump)| {
                let evented = pump.as_ref().map(|pump| {
                    (
                        triomphe::Arc::ptr_eq(&pump.shared.transport.link, &same_link),
                        pump.shared.transport.reply_budget,
                        pump.shared.transport.token,
                    )
                });
                (client.classes(), evented)
            });
            let _ = finished.send(result);
        });
        static CONNECTED: hex_core::patience::Patience =
            hex_core::patience::Patience::measuring("test.hex_client.guest.connect_completed");
        let observed = connected_by(&CONNECTED, &answer, &worker);
        let stopped = peer.shutdown(Shutdown::Both);
        drop(peer);
        let joined = worker.join();
        eprintln!(
            "client_connect_positive form={form:?} at={at:?} observed={observed:?} stopped={stopped:?} joined={}",
            joined.is_ok()
        );
        joined.unwrap();
        let (classes, evented) = observed.unwrap().unwrap();
        assert_eq!(classes, vec![Class::Oci]);
        match form {
            ConnectForm::Ordinary => assert_eq!(evented, None),
            ConnectForm::Evented => assert_eq!(evented, Some((true, None, TOKEN))),
        }
        let hello = constructor_hello(&trace);
        eprintln!("client_connect_positive_hello form={form:?} hello={hello:?}");
        assert!(!hello.pending && hello.buffered == 0);
        assert_eq!(
            hello
                .header
                .expect("positive requires a complete actual Hello")
                .id,
            1
        );
        assert_eq!(
            *trace.dials.lock().unwrap(),
            [(stream::CONTROL_PORT, Some(at))]
        );
    }
}

/// AC-T7.4.1: the actual Shards adapter rejects before effects, then carries the
/// identical absolute authority and bytes through the existing unique facade.
#[test]
fn shards_link_carries_the_original_absolute_deadline_and_bytes() {
    let guest = Arc::new(Patient {
        listener: TcpListener::bind("127.0.0.1:0").unwrap(),
        given: std::sync::Mutex::new(Vec::new()),
        absolute: std::sync::Mutex::new(Vec::new()),
    });
    let link = shards::ShardsLink::new(guest.clone(), Duration::from_secs(1));
    assert!(
        link.dial_before(stream::CONTROL_PORT, Some(Instant::now()))
            .is_err()
    );
    assert!(guest.absolute.lock().unwrap().is_empty());
    assert!(guest.given.lock().unwrap().is_empty());
    let at = Instant::now().checked_add(Duration::from_secs(1)).unwrap();
    let mut stream = link.dial_before(stream::CONTROL_PORT, Some(at)).unwrap();
    let (mut peer, _) = guest.listener.accept().unwrap();
    peer.set_read_timeout(Some(Duration::from_secs(1))).unwrap();
    stream.write_all(b"exact operation authority").unwrap();
    let mut bytes = [0u8; 25];
    peer.read_exact(&mut bytes).unwrap();
    assert_eq!(&bytes, b"exact operation authority");
    assert_eq!(*guest.absolute.lock().unwrap(), [at]);
}

/// AC-T7.4.1: moving the unique facade through the existing copier adapter clones
/// no native stream; an explicit native clone retains I/O until its own final drop.
#[test]
fn duplex_owner_preserves_io_and_explicit_native_clone_lifetime() {
    use std::sync::atomic::Ordering;

    let (socket, mut peer) = connect_pair();
    socket.set_read_timeout(Some(CONNECT_OBSERVE)).unwrap();
    socket.set_write_timeout(Some(CONNECT_OBSERVE)).unwrap();
    peer.set_read_timeout(Some(CONNECT_OBSERVE)).unwrap();
    peer.set_write_timeout(Some(CONNECT_OBSERVE)).unwrap();
    let trace = Arc::new(ConnectTrace::default());
    let owner = own_duplex(ConnectSocket {
        socket,
        trace: Arc::clone(&trace),
    })
    .unwrap();
    let before_move = std::ptr::from_ref(&*owner);
    let mut moved = DuplexIo(owner);
    let same_facade = std::ptr::addr_eq(before_move, std::ptr::from_ref(&*moved.0));
    let clones_before = trace.native_clones.load(Ordering::SeqCst);
    let input_sent = peer.write_all(b"in");
    let mut input = [0; 2];
    let input_read = moved.read_exact(&mut input);
    let output_sent = moved.write_all(b"out");
    let mut output = [0; 3];
    let output_read = peer.read_exact(&mut output);
    let clone = moved.0.try_clone();
    drop(moved);
    let first_drop = trace.facade_drops.load(Ordering::SeqCst);
    let final_write = match clone {
        Ok(mut clone) => {
            let sent = clone.write_all(b"last");
            drop(clone);
            sent
        }
        Err(why) => Err(why),
    };
    let mut last = [0; 4];
    let last_read = peer.read_exact(&mut last);
    let eof = peer.read(&mut [0]);
    drop(peer);
    let clones = trace.native_clones.load(Ordering::SeqCst);
    let drops = trace.facade_drops.load(Ordering::SeqCst);
    eprintln!(
        "client_duplex_owner same={same_facade} clones_before={clones_before} clones={clones} first_drop={first_drop} drops={drops} eof={eof:?}"
    );
    // All owned facade/peer handles are gone before any result assertion.
    input_sent.unwrap();
    input_read.unwrap();
    output_sent.unwrap();
    output_read.unwrap();
    final_write.unwrap();
    last_read.unwrap();
    assert!(same_facade);
    assert_eq!(clones_before, 0);
    assert_eq!(clones, 1);
    assert_eq!(first_drop, 1);
    assert_eq!(drops, 2);
    assert_eq!(input, *b"in");
    assert_eq!(output, *b"out");
    assert_eq!(last, *b"last");
    assert_eq!(eof.unwrap(), 0);
}

/// The relay token's read on a link with no setup bound of its own, as QUIC's bridged socket is (`hex_quic::link`),
/// against XNU's lost half-close wakeup (joint plan step 4, 2(a); Shards M236, D175).
#[cfg(unix)]
mod token_end {
    use std::net::Shutdown;
    use std::os::unix::net::UnixStream;
    use std::sync::Mutex;
    use std::sync::OnceLock;
    use std::time::Instant;

    use hex_core::patience::Patience;
    use hex_testkit::waits::answered;
    use hex_testkit::waits::follow::{self, Party, Thread};
    use rustix::event::{PollFd, PollFlags, Timespec};

    use super::*;

    // Each wait is for its own fact, named for it: no census records either yet, so each measures, for the fact alone
    // (hex-testkit's rule).
    /// The token's first byte taken by the connect: the relay's end writable again.
    static TAKEN: Patience = Patience::measuring("test.hex_client.guest.token_taken");
    /// The connect's answer once the relay has ended its half, the connecting thread its party.
    static ANSWERED: Patience = Patience::measuring("test.hex_client.guest.token_end");

    /// The rounds, and the spread of the half-close's spin, of Shards' race harness for its M236 (Shards
    /// `docs/research/platform-measurements.md` M236 and `docs/research/measurements/half-close/`, at 04abb68d), as the
    /// stdio copy's race test takes them (`copy::tests`).
    const ROUNDS: u32 = 50_000;
    const SPREAD: u32 = 400;

    /// A link whose control port is one end of a Unix socket pair, with no setup bound of its own: the relay is the
    /// test's end.
    struct Paired(Mutex<Option<UnixStream>>);

    impl Link for Paired {
        fn dial(&self, port: u32) -> Result<DuplexOwner, Refusal> {
            assert_eq!(port, stream::CONTROL_PORT);
            own_duplex(self.0.lock().unwrap().take().expect("dialed once"))
        }
    }

    /// Whether the relay's end has room to write now (poll(2)'s POLLOUT, not waiting): the connect took its byte.
    fn writable(relay: &UnixStream) -> bool {
        let mut ready = [PollFd::new(relay, PollFlags::OUT)];
        let now = Timespec {
            tv_sec: 0,
            tv_nsec: 0,
        };
        rustix::event::poll(&mut ready, Some(&now)).is_ok()
            && ready[0].revents().contains(PollFlags::OUT)
    }

    /// AC-T7.4.1 (joint plan step 4, 2(a): HEX-QUIC-25 B3's hardening; Shards M236 and D175): a relay that ends its half
    /// with its token half sent refuses the connect, however its end lands against the token's next read. Do, each
    /// round: connect through a link with no setup bound and no caller deadline (`Stream::dial`, as QUIC's link is
    /// dialed), its relay sending the token's first byte with the least send buffer its kernel keeps (a byte on macOS);
    /// as soon as the connect has taken that byte (the relay's end writable again), so its read waits again, spin the
    /// round's share of M236's spread and end the relay's half (SHUT_WR), keeping it open. Expect the connect refused,
    /// typed, naming the token, every round. On macOS a read that waits in recv(2) on a Unix stream socket sleeps through
    /// a half-close landing as XNU's `uipc_rcvd` lets go of the reader's lock (M236: 360 of 50,000 readers asleep), so
    /// the token's read waits in poll(2), which sees it. The answer's wait follows the connecting thread (decision 9):
    /// past W* it goes on while that thread runs or works, and fails only once it has slept through a whole window, as
    /// one asleep through the relay's end would.
    #[test]
    fn ac_t7_4_1_a_relay_that_ends_mid_token_refuses_the_connect() {
        for round in 0..ROUNDS {
            let (ours, mut relay) = UnixStream::pair().unwrap();
            rustix::net::sockopt::set_socket_send_buffer_size(&relay, 1).unwrap();
            relay.set_nonblocking(true).unwrap();
            let link = own_link(Paired(Mutex::new(Some(ours)))).unwrap();
            let (told, answer) = std::sync::mpsc::sync_channel(1);
            let connecting = Arc::new(OnceLock::new());
            let named = Arc::clone(&connecting);
            std::thread::spawn(move || {
                if let Ok(thread) = Thread::current() {
                    let _ = named.set(thread);
                }
                let _ = told.send(Stream::<Guest>::dial(link).map(drop));
            });
            relay.write_all(&TOKEN.to_le_bytes()[..1]).unwrap();
            // The take is looked for without giving the CPU up, so the half-close follows the read's next wait closely.
            let began = Instant::now();
            while !writable(&relay) {
                std::hint::spin_loop();
            }
            answered(&TAKEN, began.elapsed());
            for _ in 0..round % SPREAD {
                std::hint::spin_loop();
            }
            relay.shutdown(Shutdown::Write).unwrap();
            let connected = follow::within(
                &ANSWERED,
                "the connect's answer",
                || connecting.get().map_or(Party::Live, Thread::party),
                |by| match by {
                    None => answer.recv().ok(),
                    Some(at) => answer
                        .recv_timeout(at.saturating_duration_since(Instant::now()))
                        .ok(),
                },
            );
            assert!(
                matches!(&connected, Ok(Err(Refusal::Os { op, .. })) if op.contains("token")),
                "round {round}: {connected:?}"
            );
        }
    }
}

/// An output stream's own half (joint plan step 4: ROOT's decision of 2026-11-12, 1-2).
mod output_end {
    use hex_core::patience::Patience;
    use hex_testkit::waits::until;

    use super::*;

    /// An output connection's End after its name, at the agent's end.
    static ENDED: Patience = Patience::measuring("test.hex_client.guest.output_ended");

    /// AC-T7.4.1 (joint plan step 4, 2(b) and the End-then-Reset item; ROOT's decision of 2026-11-12): exec a program
    /// whose stdout is the caller's own pipe and whose stderr it asks for, through a stand-in agent that, once the exec
    /// has started, writes a line on each. Expect each output stream's connection ended on the client's side once
    /// named: the asked stderr's end takes no write, and the agent reads each connection's name, then its End. Then
    /// each line reaches the caller and the program's end comes back. A peer's End then closes the connection whole,
    /// over QUIC's bridge too, so a reader can't sleep through it (Shards M236) and the node's copier leaves no half
    /// open behind it to reset.
    #[test]
    fn an_output_streams_own_half_ends_once_named() {
        let (control, stdio, link) = listeners();
        let (checked, check) = std::sync::mpsc::channel::<()>();
        let agent = std::thread::spawn(move || {
            let (mut conn, _) = control.accept().unwrap();
            hello(&mut conn);
            let (h, request) = next_request(&mut conn);
            let Request::Call(Call::Exec { streams, .. }) = request else {
                panic!("expected an exec, got {request:?}");
            };
            assert!(!streams.stdin && streams.stdout && streams.stderr);
            let mut named = Vec::new();
            for _ in 0..2 {
                let (mut s, _) = stdio.accept().unwrap();
                let mut name = [0u8; stream::NAME_BYTES];
                s.read_exact(&mut name).unwrap();
                named.push((stream::read_name(&name).unwrap().stream, s));
            }
            send_answer(&mut conn, h.id, &Answer::Started { pid: 9 });
            check.recv().unwrap();
            for (which, s) in &mut named {
                s.set_nonblocking(true).unwrap();
                until(&ENDED, "an output connection's End after its name", || {
                    matches!(s.read(&mut [0u8; 1]), Ok(0))
                })
                .unwrap();
                s.set_nonblocking(false).unwrap();
                s.write_all(format!("{which:?}\n").as_bytes()).unwrap();
            }
            drop(named);
            send_answer(&mut conn, h.id, &Answer::Exited(Exit::Code(0)));
            // Held open until the client has its end, as an agent's connection is.
            let _ = next_request(&mut conn);
        });
        let client = Client::connect(link).unwrap();
        let (mut out_r, out_w) = std::io::pipe().unwrap();
        let mut proc = client
            .exec(
                &BoxId::new("b").unwrap(),
                Program::of(&["p"]),
                Stdio {
                    stdout: Some(out_w),
                    ask: hex_api::Ask {
                        stdin: false,
                        stdout: false,
                        stderr: true,
                    },
                    ..Stdio::null()
                },
            )
            .unwrap();
        let given = proc.streams().unwrap();
        let mut err = hex_api::OwnedStream::from(given.stderr.unwrap());
        assert!(
            err.write(b"x").is_err(),
            "the asked stderr's end took a write: the client's half of an output stream is still open"
        );
        checked.send(()).unwrap();
        let mut out = String::new();
        out_r.read_to_string(&mut out).unwrap();
        assert_eq!(out, "Out\n");
        let mut errors = String::new();
        err.read_to_string(&mut errors).unwrap();
        assert_eq!(errors, "Err\n");
        assert_eq!(proc.wait_for(Duration::MAX).unwrap(), Some(Exit::Code(0)));
        drop(proc);
        let _ = client.inspect(&BoxId::new("b").unwrap());
        agent.join().unwrap();
    }
}

/// A stream cut short (joint plan step 4, route (b): ROOT's decision of 2026-11-12, docs/design/hex-quic.md Q11).
mod cut_short {
    use std::net::Shutdown;

    use super::*;

    /// A loopback stdio connection that tells its cut as hex-quic's bridge tells a QUIC connection's
    /// (`Duplex::cut`): the test records it.
    struct Cuttable {
        conn: TcpStream,
        cut: crate::Cut,
    }

    impl Read for Cuttable {
        fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
            self.conn.read(buf)
        }
    }

    impl Write for Cuttable {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.conn.write(buf)
        }

        fn flush(&mut self) -> std::io::Result<()> {
            self.conn.flush()
        }
    }

    impl Duplex for Cuttable {
        fn try_clone(&self) -> std::io::Result<DuplexOwner> {
            own_duplex(Cuttable {
                conn: self.conn.try_clone()?,
                cut: self.cut.clone(),
            })
            .map_err(|_| io::Error::from(io::ErrorKind::OutOfMemory))
        }

        fn shutdown(&self, how: Shutdown) -> std::io::Result<()> {
            self.conn.shutdown(how)
        }

        fn set_write_timeout(&self, timeout: Option<Duration>) -> std::io::Result<()> {
            self.conn.set_write_timeout(timeout)
        }

        fn set_read_timeout(&self, timeout: Option<Duration>) -> std::io::Result<()> {
            self.conn.set_read_timeout(timeout)
        }

        fn handle(&self) -> super::super::Handle<'_> {
            Duplex::handle(&self.conn)
        }

        fn set_nonblocking(&self, on: bool) -> std::io::Result<()> {
            self.conn.set_nonblocking(on)
        }

        fn cut(&self) -> Option<crate::Cut> {
            Some(self.cut.clone())
        }
    }

    /// The guest's ports as two loopback listeners, each stdio connection's cut kept for the test to record.
    struct Cuttables {
        control: SocketAddr,
        stdio: SocketAddr,
        cuts: Arc<std::sync::Mutex<Vec<crate::Cut>>>,
    }

    impl Link for Cuttables {
        fn dial(&self, port: u32) -> Result<DuplexOwner, Refusal> {
            if port == stream::CONTROL_PORT {
                return own_duplex(TcpStream::connect(self.control).unwrap());
            }
            let cut = crate::Cut::default();
            self.cuts.lock().unwrap().push(cut.clone());
            own_duplex(Cuttable {
                conn: TcpStream::connect(self.stdio).unwrap(),
                cut,
            })
        }

        fn carries_half_close(&self) -> bool {
            true
        }
    }

    /// AC-T7.4.1 (route (b)): exec a program whose stderr the caller asks for, over a link whose stdio connections tell
    /// their cut (as QUIC's bridge does, its own test `hex_quic::tests::ac_t7_4_1_a_stream_cut_short_reads_as_cut`);
    /// the stand-in agent writes part of a line, the connection is cut (the test records a reset as the bridge would),
    /// and the agent says the program exited 0. Expect the caller to read the part, and the program's end refused
    /// Broken, naming stderr: a stream the caller reads itself is told cut by its program's end.
    #[test]
    fn an_asked_stream_cut_short_ends_its_program_broken() {
        let (control, stdio) = (
            TcpListener::bind("127.0.0.1:0").unwrap(),
            TcpListener::bind("127.0.0.1:0").unwrap(),
        );
        let cuts = Arc::new(std::sync::Mutex::new(Vec::new()));
        let link = own_link(Cuttables {
            control: control.local_addr().unwrap(),
            stdio: stdio.local_addr().unwrap(),
            cuts: Arc::clone(&cuts),
        })
        .unwrap();
        let (was_cut, cut_done) = std::sync::mpsc::channel::<()>();
        let agent = std::thread::spawn(move || {
            let (mut conn, _) = control.accept().unwrap();
            hello(&mut conn);
            let (h, _) = next_request(&mut conn);
            let (mut err, _) = stdio.accept().unwrap();
            let mut name = [0u8; stream::NAME_BYTES];
            err.read_exact(&mut name).unwrap();
            send_answer(&mut conn, h.id, &Answer::Started { pid: 12 });
            err.write_all(b"par").unwrap();
            cut_done.recv().unwrap();
            send_answer(&mut conn, h.id, &Answer::Exited(Exit::Code(0)));
            // Held open until the client has its end, as an agent's connection is.
            let _ = next_request(&mut conn);
            drop(err);
        });
        let client = Client::connect(link).unwrap();
        let mut proc = client
            .exec(
                &BoxId::new("b").unwrap(),
                Program::of(&["p"]),
                Stdio {
                    ask: hex_api::Ask {
                        stdin: false,
                        stdout: false,
                        stderr: true,
                    },
                    ..Stdio::null()
                },
            )
            .unwrap();
        let mut err = hex_api::OwnedStream::from(proc.streams().unwrap().stderr.unwrap());
        let mut part = [0u8; 3];
        err.read_exact(&mut part).unwrap();
        assert_eq!(&part, b"par");
        let cut = cuts.lock().unwrap().first().cloned().unwrap();
        cut.record(&io::Error::new(
            io::ErrorKind::ConnectionReset,
            "the connection was reset",
        ));
        was_cut.send(()).unwrap();
        let ended = proc.wait_for(Duration::MAX);
        assert!(
            matches!(&ended, Err(Refusal::Broken(why)) if why.contains("stderr")),
            "{ended:?}"
        );
        drop((proc, err));
        let _ = client.inspect(&BoxId::new("b").unwrap());
        agent.join().unwrap();
    }

    /// AC-T7.4.1 (route (b)): exec a program whose stdout is the caller's own pipe, through a stand-in agent that writes
    /// part of a line on it and, once the caller has read that part, resets the connection (an abortive close, RST),
    /// then says the program exited 0. Expect the caller's pipe to read the part, then its end, and the program's end
    /// to be refused Broken, naming stdout: a stream cut short is told, never read as complete.
    #[test]
    fn a_carried_stream_cut_short_ends_its_program_broken() {
        let (control, stdio, link) = listeners();
        let (read_part, part_read) = std::sync::mpsc::channel::<()>();
        let agent = std::thread::spawn(move || {
            let (mut conn, _) = control.accept().unwrap();
            hello(&mut conn);
            let (h, _) = next_request(&mut conn);
            let (mut out, _) = stdio.accept().unwrap();
            let mut name = [0u8; stream::NAME_BYTES];
            out.read_exact(&mut name).unwrap();
            send_answer(&mut conn, h.id, &Answer::Started { pid: 11 });
            out.write_all(b"par").unwrap();
            part_read.recv().unwrap();
            rustix::net::sockopt::set_socket_linger(&out, Some(Duration::ZERO)).unwrap();
            drop(out);
            send_answer(&mut conn, h.id, &Answer::Exited(Exit::Code(0)));
            // Held open until the client has its end, as an agent's connection is.
            let _ = next_request(&mut conn);
        });
        let client = Client::connect(link).unwrap();
        let (mut out_r, out_w) = std::io::pipe().unwrap();
        let mut proc = client
            .exec(
                &BoxId::new("b").unwrap(),
                Program::of(&["p"]),
                Stdio {
                    stdout: Some(out_w),
                    ..Stdio::null()
                },
            )
            .unwrap();
        let mut part = [0u8; 3];
        out_r.read_exact(&mut part).unwrap();
        assert_eq!(&part, b"par");
        read_part.send(()).unwrap();
        let mut rest = Vec::new();
        out_r.read_to_end(&mut rest).unwrap();
        assert!(rest.is_empty(), "{rest:?}");
        let ended = proc.wait_for(Duration::MAX);
        assert!(
            matches!(&ended, Err(Refusal::Broken(why)) if why.contains("stdout")),
            "{ended:?}"
        );
        drop(proc);
        let _ = client.inspect(&BoxId::new("b").unwrap());
        agent.join().unwrap();
    }
}
