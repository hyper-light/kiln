//! Ports of this machine, opened for the server (docs/previews.md, "On your device"): a person's
//! own device opens an agent's dev server through kiln, and kiln's server reaches it here. Each
//! connection is to a port this machine's processes listen on, on loopback (`127.0.0.1`, else
//! `::1`), and only one the session's machine lists ([`allowed`]: its user's own, none of the
//! system's). Its bytes go both ways over the runner's link on a lane of their own
//! (`LANE_PORTS`): no more than [`WINDOW`] of the port's the server hasn't taken, so a large file
//! never fills the server's memory, and what the server sends (a browser's request, a
//! WebSocket's messages) written as it comes.

use std::collections::HashMap;
use std::net::{Ipv4Addr, Ipv6Addr, SocketAddr};
use std::sync::OnceLock;
use std::time::{Duration, Instant};

use bytes::Bytes;
use futures::io::{AsyncReadExt as _, AsyncWriteExt as _};
use futures::{FutureExt as _, StreamExt as _};
use parking_lot::Mutex;

use crate::exec::Up;
use crate::proto::AgentMsg;

/// Bytes a connection's port may send the server before the server says it took them.
pub const WINDOW: u64 = 4 << 20;
/// The most of a connection's bytes in one message.
pub const CHUNK: usize = 64 << 10;
/// How long reaching a port may take.
const CONNECT: Duration = Duration::from_secs(5);
/// How long a session's list of ports is good for (a connection per request of a page).
const LISTED_FOR: Duration = Duration::from_secs(5);

/// A connection, as this side needs it: what goes to its port, the server's acknowledgements, and
/// its end (dropped, the connection's reader stops).
struct Near {
    to_port: async_channel::Sender<Bytes>,
    acks: futures::channel::mpsc::UnboundedSender<u64>,
    _stop: async_channel::Sender<()>,
}

fn near() -> &'static Mutex<HashMap<u64, Near>> {
    static NEAR: OnceLock<Mutex<HashMap<u64, Near>>> = OnceLock::new();
    NEAR.get_or_init(Mutex::default)
}

/// Whether port `port` may be opened for session `session` (its harness's process `root`): one its
/// machine lists for it (`super::apps::listening`), read again at most every few seconds.
pub fn allowed(session: &str, root: Option<u32>, port: u16) -> bool {
    static LISTED: OnceLock<Mutex<HashMap<String, (Instant, Vec<u16>)>>> = OnceLock::new();
    let listed = LISTED.get_or_init(Mutex::default);
    if let Some((at, ports)) = listed.lock().get(session)
        && at.elapsed() < LISTED_FOR
        && ports.contains(&port)
    {
        return true;
    }
    let ports: Vec<u16> = super::apps::listening(root).iter().map(|l| l.port).collect();
    let ok = ports.contains(&port);
    let mut g = listed.lock();
    g.retain(|_, (at, _)| at.elapsed() < LISTED_FOR);
    g.insert(session.to_owned(), (Instant::now(), ports));
    ok
}

/// `ServerMsg::PortOpen`: connect `conn` to `port` (when `allowed`), say so, then carry its bytes
/// until either side ends it.
pub async fn serve_open(conn: u64, port: u16, allowed: bool, up: async_channel::Sender<Up>) {
    if !allowed {
        say(&up, AgentMsg::PortOpened { conn, error: Some(format!("nothing of this session's machine's own listens on port {port}")) }).await;
        return;
    }
    let tcp = match connect(port).await {
        Ok(t) => t,
        Err(e) => {
            say(&up, AgentMsg::PortOpened { conn, error: Some(format!("port {port}: {e:#}")) }).await;
            return;
        }
    };
    let (mut rd, mut wr) = crate::http::io::RtFuturesIo::new(tcp).split();
    let (to_port, from_server) = async_channel::unbounded::<Bytes>();
    let (acks, mut acked) = futures::channel::mpsc::unbounded::<u64>();
    let (stop, stopped) = async_channel::bounded::<()>(1);
    near().lock().insert(conn, Near { to_port, acks, _stop: stop });
    if !say(&up, AgentMsg::PortOpened { conn, error: None }).await {
        near().lock().remove(&conn);
        return;
    }
    // What the server sends, to the port, as it comes; each piece taken, acknowledged.
    let ack = up.clone();
    crate::rt::spawn(async move {
        while let Ok(b) = from_server.recv().await {
            if wr.write_all(&b).await.is_err() {
                break;
            }
            let bytes = u64::try_from(b.len()).unwrap_or(u64::MAX);
            if ack.send(Up::Port(AgentMsg::PortAck { conn, bytes })).await.is_err() {
                break;
            }
        }
        let _ = wr.close().await;
    });
    // What the port sends, to the server, within the window.
    crate::rt::spawn(async move {
        let up = up;
        let mut buf = vec![0u8; CHUNK];
        let mut unacked = 0u64;
        let why = loop {
            while let Ok(n) = acked.try_recv() {
                unacked = unacked.saturating_sub(n);
            }
            if unacked >= WINDOW {
                match acked.next().await {
                    Some(n) => {
                        unacked = unacked.saturating_sub(n);
                        continue;
                    }
                    None => break None,
                }
            }
            let read = futures::select_biased! {
                _ = stopped.recv().fuse() => break None,
                r = rd.read(&mut buf).fuse() => r,
            };
            match read {
                Ok(0) => break None,
                Ok(n) => {
                    let Some(data) = buf.get(..n) else { break None };
                    unacked += u64::try_from(n).unwrap_or(u64::MAX);
                    if !say(&up, AgentMsg::PortData { conn, data: data.to_vec() }).await {
                        break None;
                    }
                }
                Err(e) => break Some(e.to_string()),
            }
        };
        // Ended here (not by the server, whose end removed it already): said.
        if near().lock().remove(&conn).is_some() {
            say(&up, AgentMsg::PortClose { conn, error: why }).await;
        }
    });
}

/// Said to the server, on the ports' lane: whether it went.
async fn say(up: &async_channel::Sender<Up>, m: AgentMsg) -> bool {
    up.send(Up::Port(m)).await.is_ok()
}

/// `port` on loopback: IPv4's first, then IPv6's (a dev server asked for `localhost` often listens
/// on `::1` alone).
async fn connect(port: u16) -> anyhow::Result<hyper_rt::tcp::TcpStream> {
    let mut last = anyhow::anyhow!("nothing listens there");
    for addr in [SocketAddr::from((Ipv4Addr::LOCALHOST, port)), SocketAddr::from((Ipv6Addr::LOCALHOST, port))] {
        match crate::rt::timeout(CONNECT, hyper_rt::tcp::TcpStream::connect(addr)).await {
            Ok(Ok(t)) => return Ok(t),
            Ok(Err(e)) => last = anyhow::anyhow!("{e}"),
            Err(_) => last = anyhow::anyhow!("it didn't answer in {}s", CONNECT.as_secs()),
        }
    }
    Err(last)
}

/// `ServerMsg::PortData`: to the connection's port.
pub fn data(conn: u64, data: Vec<u8>) {
    if let Some(n) = near().lock().get(&conn) {
        let _ = n.to_port.try_send(Bytes::from(data));
    }
}

/// `ServerMsg::PortAck`: the server took that many more of the port's bytes.
pub fn ack(conn: u64, bytes: u64) {
    if let Some(n) = near().lock().get(&conn) {
        let _ = n.acks.unbounded_send(bytes);
    }
}

/// `ServerMsg::PortClose`: the server is done with it (what's still to write goes first).
pub fn close(conn: u64) {
    near().lock().remove(&conn);
}

#[cfg(test)]
mod tests {
    use super::*;

    /// What a connection says, by kind: what it read, whether it opened, its acknowledgements.
    async fn said(rx: &async_channel::Receiver<Up>) -> Option<AgentMsg> {
        match crate::rt::timeout(Duration::from_secs(10), rx.recv()).await {
            Ok(Ok(Up::Port(m))) => Some(m),
            _ => None,
        }
    }

    #[test]
    fn a_connection_carries_bytes_both_ways_and_ends_with_its_port() -> anyhow::Result<()> {
        crate::rt::block_on(async {
            // A port that answers what it's sent, then hangs up.
            let l = std::net::TcpListener::bind("127.0.0.1:0")?;
            let port = l.local_addr()?.port();
            std::thread::spawn(move || {
                use std::io::{Read as _, Write as _};
                if let Ok((mut s, _)) = l.accept() {
                    let mut b = [0u8; 5];
                    if s.read_exact(&mut b).is_ok() {
                        let _ = s.write_all(&[b"hello ".as_slice(), &b].concat());
                    }
                }
            });
            let (up, rx) = async_channel::unbounded();
            crate::rt::spawn(serve_open(7, port, true, up));
            assert_eq!(said(&rx).await, Some(AgentMsg::PortOpened { conn: 7, error: None }));
            data(7, b"world".to_vec());
            let mut got = Vec::new();
            let mut acked = 0;
            let mut closed = false;
            while !closed {
                match said(&rx).await {
                    Some(AgentMsg::PortData { conn: 7, data }) => got.extend(data),
                    Some(AgentMsg::PortAck { conn: 7, bytes }) => acked += bytes,
                    Some(AgentMsg::PortClose { conn: 7, .. }) => closed = true,
                    other => anyhow::bail!("unexpected {other:?}"),
                }
            }
            assert_eq!(got, b"hello world");
            assert_eq!(acked, 5, "what the server sent was acknowledged");
            assert!(near().lock().get(&7).is_none(), "ended, it's gone");
            Ok(())
        })
    }

    #[test]
    fn a_port_not_listed_or_not_listening_isnt_opened() -> anyhow::Result<()> {
        crate::rt::block_on(async {
            let (up, rx) = async_channel::unbounded();
            serve_open(8, 1, false, up.clone()).await;
            assert!(matches!(said(&rx).await, Some(AgentMsg::PortOpened { conn: 8, error: Some(_) })));
            // Listed but nothing there any more.
            let gone = std::net::TcpListener::bind("127.0.0.1:0")?.local_addr()?.port();
            serve_open(9, gone, true, up).await;
            assert!(matches!(said(&rx).await, Some(AgentMsg::PortOpened { conn: 9, error: Some(_) })));
            Ok(())
        })
    }

    #[test]
    fn only_ports_its_machine_lists_are_allowed() -> anyhow::Result<()> {
        let l = std::net::TcpListener::bind("127.0.0.1:0")?;
        let port = l.local_addr()?.port();
        // This process's own ports are kiln's, never listed (`super::super::apps::listening`).
        assert!(!allowed("s-ports", Some(std::process::id()), port));
        assert!(!allowed("s-ports", None, 22));
        Ok(())
    }
}
