//! The front door's DNS server: UDP and TCP (RFC 1035 §4.2) on each address it listens on,
//! answering from the [`Zone`] and forwarding the rest upstream in resolver mode.
//!
//! * **Answers** come from the zone, with the address of the interface the query arrived on
//!   (the socket's own address, or for a wildcard socket this machine's address on the route to
//!   the asker).
//! * **Forwarding** relays the query's bytes, never re-encoded (records kiln does not interpret
//!   keep their compression pointers), from a fresh socket with a new random id, to each upstream
//!   in turn (1.5 s each); the answer goes back with the asker's id. A truncated UDP answer is
//!   passed on as it is: the asker retries over TCP, which is forwarded over TCP. At most
//!   [`MAX_FORWARDS`] are in flight; past that, and when no upstream answers, SERVFAIL.
//! * **Limits.** Datagrams up to 4 KiB in; answers over the asker's UDP size are sent truncated;
//!   TCP connections at most [`MAX_TCP`] at once, 10 s idle, 100 queries each.

use std::net::{IpAddr, SocketAddr};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::time::Duration;

use anyhow::Context as _;
use futures::FutureExt as _;
use parking_lot::RwLock;

use crate::discovery::Iface;
use crate::dns::{self, Message};
use crate::rt::Shutdown;

use super::zone::{Answer, Zone};

/// Forwarded queries in flight at once (per listening address).
pub const MAX_FORWARDS: usize = 256;
/// TCP connections at once (per listening address).
pub const MAX_TCP: usize = 64;
const UPSTREAM_TIMEOUT: Duration = Duration::from_millis(1500);
const TCP_IDLE: Duration = Duration::from_secs(10);
const TCP_QUERIES: usize = 100;

/// What every listener of one DNS server reads: the zone, where to forward, this machine's
/// interfaces; and what they count.
#[derive(Default)]
pub struct Shared {
    pub zone: RwLock<Zone>,
    pub upstream: RwLock<Vec<SocketAddr>>,
    pub ifaces: RwLock<Vec<Iface>>,
    /// Queries answered (from the zone, forwarded or refused).
    pub served: AtomicU64,
}

impl Shared {
    fn facing(&self, local: SocketAddr, from: SocketAddr) -> super::zone::Facing {
        let ip = if local.ip().is_unspecified() {
            crate::discovery::local_address_for(from.ip()).unwrap_or(local.ip())
        } else {
            local.ip()
        };
        super::netinfo::facing(ip, &self.ifaces.read())
    }

    /// Recursion only for askers on a private network, or on one of this machine's own subnets
    /// (a LAN with public IPv6 addresses): never an open resolver for the internet.
    fn may_recurse(&self, from: IpAddr) -> bool {
        if crate::discovery::is_private(from) {
            return true;
        }
        self.ifaces.read().iter().any(|i| match from {
            IpAddr::V4(a) => i.v4.iter().any(|(x, m)| u32::from(*x) & u32::from(*m) == u32::from(a) & u32::from(*m)),
            IpAddr::V6(a) => i.v6.iter().any(|x| crate::discovery::in_net(IpAddr::V6(a), IpAddr::V6(*x), 64)),
        })
    }
}

/// A running DNS server: a listener per address, each stopped when it is no longer wanted or
/// the server is dropped.
pub struct DnsServer {
    shared: Arc<Shared>,
    listening: Vec<(SocketAddr, Shutdown)>,
    /// Where it could not listen, last time it tried.
    pub failed: Vec<(SocketAddr, std::io::Error)>,
}

impl Drop for DnsServer {
    fn drop(&mut self) {
        for (_, stop) in &self.listening {
            stop.cancel();
        }
    }
}

impl DnsServer {
    /// Listen on each of `binds` that can be bound (UDP and TCP both); the others are reported.
    pub fn start(binds: &[SocketAddr], shared: &Arc<Shared>) -> Self {
        let mut s = Self { shared: Arc::clone(shared), listening: Vec::new(), failed: Vec::new() };
        s.ensure(binds);
        s
    }

    /// Listen on exactly `binds`: stop the listeners on other addresses and start the missing
    /// ones (those that fail are in [`Self::failed`], and tried again on the next call).
    pub fn ensure(&mut self, binds: &[SocketAddr]) {
        self.listening.retain(|(addr, stop)| {
            let keep = binds.contains(addr);
            if !keep {
                stop.cancel();
            }
            keep
        });
        self.failed.clear();
        for addr in binds {
            if self.listening.iter().any(|(a, _)| a == addr) {
                continue;
            }
            match bind(*addr) {
                Ok((udp, tcp)) => {
                    let stop = Shutdown::new();
                    let (sh, sd, local) = (Arc::clone(&self.shared), stop.clone(), *addr);
                    crate::rt::spawn(async move { serve_udp(udp, local, sh, sd).await });
                    let (sh, sd) = (Arc::clone(&self.shared), stop.clone());
                    crate::rt::spawn(async move { serve_tcp(tcp, local, sh, sd).await });
                    self.listening.push((*addr, stop));
                }
                Err(e) => self.failed.push((*addr, e)),
            }
        }
    }

    /// Where it listens (UDP and TCP).
    pub fn bound(&self) -> Vec<SocketAddr> {
        self.listening.iter().map(|(a, _)| *a).collect()
    }
}

/// The UDP socket and the TCP listener for one address: both, or neither.
fn bind(addr: SocketAddr) -> std::io::Result<(std::net::UdpSocket, std::net::TcpListener)> {
    use socket2::{Domain, Protocol, Socket, Type};
    let domain = if addr.is_ipv4() { Domain::IPV4 } else { Domain::IPV6 };
    let udp = Socket::new(domain, Type::DGRAM, Some(Protocol::UDP))?;
    if addr.is_ipv6() {
        udp.set_only_v6(true)?;
    }
    udp.bind(&addr.into())?;
    let tcp = Socket::new(domain, Type::STREAM, Some(Protocol::TCP))?;
    if addr.is_ipv6() {
        tcp.set_only_v6(true)?;
    }
    // A restart rebinds at once despite connections in TIME_WAIT.
    tcp.set_reuse_address(true)?;
    tcp.bind(&addr.into())?;
    tcp.listen(64)?;
    Ok((udp.into(), tcp.into()))
}

/// The id of a DNS message, and the message with another id.
fn with_id(data: &[u8], id: u16) -> Vec<u8> {
    let mut v = data.to_vec();
    if let Some(head) = v.get_mut(..2) {
        head.copy_from_slice(&id.to_be_bytes());
    }
    v
}

/// The largest answer the asker takes over UDP: its EDNS(0) size (at most 4096), else 512.
fn udp_limit(q: &Message) -> usize {
    q.additionals.iter().find(|r| r.rtype() == dns::TYPE_OPT).map_or(512, |r| usize::from(r.class).clamp(512, 4096))
}

/// Encode an answer for UDP: one that does not fit goes without its records, marked truncated.
fn encode_udp(m: &Message, limit: usize) -> Option<Vec<u8>> {
    let bytes = m.encode().ok()?;
    if bytes.len() <= limit {
        return Some(bytes);
    }
    let cut = Message { flags: m.flags | dns::FLAG_TRUNCATED, answers: Vec::new(), authorities: Vec::new(), ..m.clone() };
    cut.encode().ok()
}

/// Forward `query` (its bytes, as asked) to the upstreams in turn: the first answer, with the
/// asker's id again.
async fn forward(upstreams: &[SocketAddr], query: &[u8], tcp: bool) -> Option<Vec<u8>> {
    let asked = u16::from_be_bytes([*query.first()?, *query.get(1)?]);
    for server in upstreams {
        let id: u16 = rand::random();
        let q = with_id(query, id);
        let got = if tcp {
            crate::discovery::unicast::exchange_tcp(*server, &q, id, crate::discovery::unicast::TCP_TIMEOUT).await
        } else {
            crate::discovery::unicast::exchange_udp(*server, &q, id, UPSTREAM_TIMEOUT).await
        };
        match got {
            Ok(answer) => return Some(with_id(&answer, asked)),
            Err(e) => tracing::debug!(%server, error = %e, "upstream resolver did not answer"),
        }
    }
    None
}

async fn serve_udp(sock: std::net::UdpSocket, local: SocketAddr, shared: Arc<Shared>, stop: Shutdown) {
    let sock = match hyper_rt::udp::UdpSocket::adopt(sock.into()) {
        Ok(s) => s,
        Err(e) => {
            tracing::warn!(%local, error = %e, "DNS: cannot hand the UDP socket to the runtime");
            return;
        }
    };
    // Forwarded answers come back through here: the socket stays this task's own.
    let (tx, rx) = async_channel::bounded::<(Vec<u8>, SocketAddr)>(MAX_FORWARDS);
    let in_flight = Arc::new(AtomicUsize::new(0));
    let mut buf = vec![0u8; 4096];
    let mut errors = 0u32;
    loop {
        enum Ev {
            Stop,
            Packet(Result<(usize, SocketAddr), hyper_rt::RtError>),
            Reply((Vec<u8>, SocketAddr)),
        }
        let ev = {
            let packet = sock.recv_from(&mut buf).fuse();
            let reply = rx.recv().fuse();
            let stopped = stop.cancelled().fuse();
            futures::pin_mut!(packet, reply, stopped);
            futures::select_biased! {
                () = stopped => Ev::Stop,
                r = reply => match r { Ok(r) => Ev::Reply(r), Err(_) => Ev::Stop },
                p = packet => Ev::Packet(p),
            }
        };
        match ev {
            Ev::Stop => return,
            Ev::Reply((bytes, to)) => {
                let _ = sock.try_send_to(&bytes, to);
            }
            Ev::Packet(Err(e)) => {
                errors += 1;
                if errors > 50 {
                    tracing::warn!(%local, error = %e, "DNS: the UDP socket keeps failing; it stops");
                    return;
                }
                crate::rt::sleep(Duration::from_millis(20)).await;
            }
            Ev::Packet(Ok((n, from))) => {
                errors = 0;
                let data = buf.get(..n).unwrap_or_default();
                let Ok(q) = Message::decode(data) else { continue };
                let verdict = shared.zone.read().answer(&q, &shared.facing(local, from), shared.may_recurse(from.ip()));
                shared.served.fetch_add(1, Ordering::Relaxed);
                match verdict {
                    Answer::Drop => {}
                    Answer::Reply(m) => {
                        if let Some(bytes) = encode_udp(&m, udp_limit(&q)) {
                            let _ = sock.try_send_to(&bytes, from);
                        }
                    }
                    Answer::Forward => {
                        if in_flight.load(Ordering::Relaxed) >= MAX_FORWARDS {
                            if let Some(bytes) = encode_udp(&super::zone::servfail(&q), 512) {
                                let _ = sock.try_send_to(&bytes, from);
                            }
                            continue;
                        }
                        in_flight.fetch_add(1, Ordering::Relaxed);
                        let upstreams = shared.upstream.read().clone();
                        let (tx, n_in_flight, query) = (tx.clone(), Arc::clone(&in_flight), data.to_vec());
                        crate::rt::spawn(async move {
                            let answer = match forward(&upstreams, &query, false).await {
                                Some(a) => Some(a),
                                None => encode_udp(&super::zone::servfail(&q), 512),
                            };
                            if let Some(a) = answer {
                                let _ = tx.send((a, from)).await;
                            }
                            n_in_flight.fetch_sub(1, Ordering::Relaxed);
                        });
                    }
                }
            }
        }
    }
}

async fn serve_tcp(listener: std::net::TcpListener, local: SocketAddr, shared: Arc<Shared>, stop: Shutdown) {
    let listener = match hyper_rt::tcp::TcpListener::from_owned(hyper_rt::tcp::OwnedStream::from(listener), 64) {
        Ok(l) => l,
        Err(e) => {
            tracing::warn!(%local, error = %e, "DNS: cannot hand the TCP listener to the runtime");
            return;
        }
    };
    let open = Arc::new(AtomicUsize::new(0));
    loop {
        let accepted = futures::select_biased! {
            () = stop.cancelled().fuse() => return,
            r = listener.accept().fuse() => r,
        };
        let stream = match accepted {
            Ok(s) => s,
            Err(e) => {
                tracing::debug!(%local, error = %e, "DNS: accept failed");
                crate::rt::sleep(Duration::from_millis(100)).await;
                continue;
            }
        };
        if open.load(Ordering::Relaxed) >= MAX_TCP {
            continue;
        }
        open.fetch_add(1, Ordering::Relaxed);
        let (sh, sd, n_open) = (Arc::clone(&shared), stop.clone(), Arc::clone(&open));
        crate::rt::spawn(async move {
            let _ = crate::discovery::until_stopped(&sd, async {
                if let Err(e) = tcp_connection(&stream, local, &sh).await {
                    tracing::trace!(error = %format!("{e:#}"), "DNS over TCP connection ended");
                }
            })
            .await;
            n_open.fetch_sub(1, Ordering::Relaxed);
        });
    }
}

async fn read_exact(stream: &hyper_rt::tcp::TcpStream, buf: &mut [u8]) -> anyhow::Result<bool> {
    let mut at = 0;
    while at < buf.len() {
        let n = stream.read(buf.get_mut(at..).unwrap_or_default()).await.map_err(|e| anyhow::anyhow!("read: {e}"))?;
        if n == 0 {
            anyhow::ensure!(at == 0, "the connection ended inside a message");
            return Ok(false);
        }
        at += n;
    }
    Ok(true)
}

/// Queries over one TCP connection, each with its two-byte length (RFC 1035 §4.2.2).
async fn tcp_connection(stream: &hyper_rt::tcp::TcpStream, local: SocketAddr, shared: &Shared) -> anyhow::Result<()> {
    let from = stream.peer_addr().map_err(|e| anyhow::anyhow!("peer: {e}"))?;
    for _ in 0..TCP_QUERIES {
        let mut head = [0u8; 2];
        match crate::rt::timeout(TCP_IDLE, read_exact(stream, &mut head)).await {
            Ok(Ok(true)) => {}
            Ok(Ok(false)) | Err(_) => return Ok(()),
            Ok(Err(e)) => return Err(e),
        }
        let mut body = vec![0u8; usize::from(u16::from_be_bytes(head))];
        anyhow::ensure!(crate::rt::timeout(TCP_IDLE, read_exact(stream, &mut body)).await.context("query timed out")??, "the connection ended");
        let Ok(q) = Message::decode(&body) else { return Ok(()) };
        let verdict = shared.zone.read().answer(&q, &shared.facing(local, from), shared.may_recurse(from.ip()));
        shared.served.fetch_add(1, Ordering::Relaxed);
        let answer = match verdict {
            Answer::Drop => continue,
            Answer::Reply(m) => m.encode().ok(),
            Answer::Forward => {
                let upstreams = shared.upstream.read().clone();
                match forward(&upstreams, &body, true).await {
                    Some(a) => Some(a),
                    None => super::zone::servfail(&q).encode().ok(),
                }
            }
        };
        let Some(answer) = answer else { continue };
        let len = u16::try_from(answer.len()).context("answer too large")?;
        let mut framed = Vec::with_capacity(answer.len() + 2);
        framed.extend_from_slice(&len.to_be_bytes());
        framed.extend_from_slice(&answer);
        stream.write_all(&framed).await.map_err(|e| anyhow::anyhow!("write: {e}"))?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::net::{Ipv4Addr, Ipv6Addr};

    use super::*;
    use crate::dns::{Name, RData};
    use crate::frontdoor::{Dns, DnsMode, Settings};

    fn shared(mode: DnsMode, upstream: Vec<SocketAddr>) -> Arc<Shared> {
        let s = Settings { domain: "kiln.test".into(), aliases: vec!["ci.test".into()], dns: Dns { enabled: true, mode, ..Dns::default() }, ..Settings::default() };
        Arc::new(Shared { zone: RwLock::new(Zone::new(&s)), upstream: RwLock::new(upstream), ..Shared::default() })
    }

    async fn ask(server: SocketAddr, name: &str, qtype: u16) -> anyhow::Result<Message> {
        let id: u16 = rand::random();
        let mut q = super::super::zone::query(name, qtype, true);
        q.id = id;
        let bytes = crate::discovery::unicast::exchange_udp(server, &q.encode()?, id, Duration::from_secs(3)).await.map_err(anyhow::Error::msg)?;
        Ok(Message::decode(&bytes)?)
    }

    /// A stand-in upstream resolver on loopback: answers every A query with 203.0.113.9.
    fn fake_upstream() -> anyhow::Result<SocketAddr> {
        let sock = std::net::UdpSocket::bind((Ipv4Addr::LOCALHOST, 0))?;
        let addr = sock.local_addr()?;
        std::thread::spawn(move || {
            let mut buf = [0u8; 1500];
            while let Ok((n, from)) = sock.recv_from(&mut buf) {
                let Ok(q) = Message::decode(buf.get(..n).unwrap_or_default()) else { continue };
                let answers = q.questions.iter().map(|qq| crate::dns::Record::new(qq.name.clone(), 30, false, RData::A(Ipv4Addr::new(203, 0, 113, 9)))).collect();
                let m = Message { id: q.id, flags: dns::FLAG_RESPONSE | dns::FLAG_RECURSION_DESIRED | 0x0080, questions: q.questions.clone(), answers, ..Message::default() };
                if let Ok(b) = m.encode() {
                    let _ = sock.send_to(&b, from);
                }
            }
        });
        Ok(addr)
    }

    #[test]
    fn answers_forwards_and_refuses_over_udp_and_tcp() -> anyhow::Result<()> {
        crate::rt::block_on(async {
            let upstream = fake_upstream()?;
            // A port free on both protocols, and no other test's, then listen there.
            let port = crate::frontdoor::test_port()?;
            let addr = SocketAddr::from((Ipv4Addr::LOCALHOST, port));
            let sh = shared(DnsMode::Resolver, vec![upstream]);
            let server = DnsServer::start(&[addr], &sh);
            anyhow::ensure!(server.failed.is_empty(), "bind: {:?}", server.failed);

            // Its own names, with the address the query came to.
            let m = ask(addr, "kiln.test", dns::TYPE_A).await?;
            assert_eq!(m.answers.first().map(|r| r.data.clone()), Some(RData::A(Ipv4Addr::LOCALHOST)));
            let m = ask(addr, "www.kiln.test", dns::TYPE_AAAA).await?;
            assert_eq!(m.answers.first().map(|r| r.data.clone()), Some(RData::Aaaa(Ipv6Addr::LOCALHOST)));
            // Anything else goes upstream, and comes back with our id.
            let m = ask(addr, "example.com", dns::TYPE_A).await?;
            assert_eq!(m.answers.first().map(|r| r.data.clone()), Some(RData::A(Ipv4Addr::new(203, 0, 113, 9))));
            assert_eq!(m.questions.first().map(|q| q.name.clone()), Some(Name::parse("example.com")));

            // Over TCP, the same answers (two queries on one connection).
            let stream = hyper_rt::tcp::TcpStream::connect(addr).await.map_err(|e| anyhow::anyhow!("{e}"))?;
            for (i, name) in ["ci.test", "kiln.test"].iter().enumerate() {
                let mut q = super::super::zone::query(name, dns::TYPE_A, true);
                q.id = u16::try_from(i)? + 100;
                let b = q.encode()?;
                let mut framed = u16::try_from(b.len())?.to_be_bytes().to_vec();
                framed.extend_from_slice(&b);
                stream.write_all(&framed).await.map_err(|e| anyhow::anyhow!("{e}"))?;
                let mut head = [0u8; 2];
                anyhow::ensure!(read_exact(&stream, &mut head).await?, "closed");
                let mut body = vec![0u8; usize::from(u16::from_be_bytes(head))];
                anyhow::ensure!(read_exact(&stream, &mut body).await?, "closed");
                let m = Message::decode(&body)?;
                assert_eq!(m.id, q.id);
                assert_eq!(m.answers.len(), 1, "{name}");
            }

            // Authoritative: the rest is refused, the domain still answered.
            *sh.zone.write() = Zone::new(&Settings {
                domain: "kiln.test".into(),
                dns: Dns { enabled: true, mode: DnsMode::Authoritative, ..Dns::default() },
                ..Settings::default()
            });
            let m = ask(addr, "example.com", dns::TYPE_A).await?;
            assert_eq!(m.rcode(), 5, "REFUSED");
            assert!(m.answers.is_empty());
            assert_eq!(ask(addr, "kiln.test", dns::TYPE_A).await?.answers.len(), 1);
            assert!(sh.served.load(Ordering::Relaxed) >= 6);

            // No upstream answering: SERVFAIL, not silence.
            *sh.zone.write() = Zone::new(&Settings { domain: "kiln.test".into(), ..Settings::default() });
            *sh.upstream.write() = vec![SocketAddr::from((Ipv4Addr::new(127, 0, 0, 1), 9))];
            let m = ask(addr, "example.org", dns::TYPE_A).await?;
            assert_eq!(m.rcode(), 2, "SERVFAIL");

            // A second server on the same address fails to bind, and says so.
            let mut again = DnsServer::start(&[addr], &sh);
            assert_eq!(again.failed.len(), 1);
            assert!(again.bound().is_empty());
            // Stopped, the address is free again: the next try binds it.
            assert_eq!(server.bound(), [addr]);
            drop(server);
            let mut bound = false;
            for _ in 0..50 {
                again.ensure(&[addr]);
                if again.bound() == [addr] {
                    bound = true;
                    break;
                }
                crate::rt::sleep(Duration::from_millis(20)).await;
            }
            assert!(bound, "rebinding after a stop");
            assert_eq!(ask(addr, "kiln.test", dns::TYPE_A).await?.answers.len(), 1);
            again.ensure(&[]);
            assert!(again.bound().is_empty());
            Ok(())
        })
    }

    #[test]
    fn oversized_answers_are_truncated_for_udp() -> anyhow::Result<()> {
        let q = super::super::zone::query("kiln.test", dns::TYPE_A, true);
        let big = Message {
            id: q.id,
            flags: dns::FLAG_RESPONSE,
            questions: q.questions.clone(),
            answers: (0..60).map(|i| crate::dns::Record::new(Name::parse("kiln.test"), 60, false, RData::A(Ipv4Addr::new(10, 0, 0, i)))).collect(),
            ..Message::default()
        };
        let b = encode_udp(&big, udp_limit(&q)).ok_or_else(|| anyhow::anyhow!("encode"))?;
        assert!(b.len() <= 512);
        let m = Message::decode(&b)?;
        assert!(m.is_truncated() && m.answers.is_empty());
        assert_eq!(with_id(&b, 0x1234).get(..2), Some(&[0x12, 0x34][..]));
        Ok(())
    }
}
