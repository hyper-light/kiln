//! WebSockets: upgrades on top of the hyper server (the RFC 6455 handshake is answered here) and
//! client connections over kiln's own TCP / TLS setup (agents' HTTP tunnel, `crate::tunnel`). Both
//! are framed by `async-tungstenite` (runtime-free; driven by hyper-rt).

use std::pin::Pin;
use std::task::{Context, Poll};

use http::{StatusCode, header};
use hyper::upgrade::Upgraded;

use super::io::RtFuturesIo;
use super::server::{Request, Response, empty};
use super::tls::{ClientTls, TlsStream};

pub use tungstenite::Message;
use tungstenite::protocol::WebSocketConfig;

/// A server-side WebSocket over an upgraded hyper connection.
pub type WebSocket = async_tungstenite::WebSocketStream<FuturesIo<Upgraded>>;

/// Adapts hyper's IO traits to `futures-io`, which async-tungstenite speaks. Safe code only: the
/// caller's buffer is already initialised, so hyper's `ReadBuf` can wrap it directly.
pub struct FuturesIo<T>(pub T);

impl<T: hyper::rt::Read + Unpin> futures::io::AsyncRead for FuturesIo<T> {
    fn poll_read(mut self: Pin<&mut Self>, cx: &mut Context<'_>, buf: &mut [u8]) -> Poll<std::io::Result<usize>> {
        let mut rb = hyper::rt::ReadBuf::new(buf);
        match Pin::new(&mut self.0).poll_read(cx, rb.unfilled()) {
            Poll::Ready(Ok(())) => Poll::Ready(Ok(rb.filled().len())),
            Poll::Ready(Err(e)) => Poll::Ready(Err(e)),
            Poll::Pending => Poll::Pending,
        }
    }
}

impl<T: hyper::rt::Write + Unpin> futures::io::AsyncWrite for FuturesIo<T> {
    fn poll_write(mut self: Pin<&mut Self>, cx: &mut Context<'_>, buf: &[u8]) -> Poll<std::io::Result<usize>> {
        Pin::new(&mut self.0).poll_write(cx, buf)
    }

    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        Pin::new(&mut self.0).poll_flush(cx)
    }

    fn poll_close(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        Pin::new(&mut self.0).poll_shutdown(cx)
    }
}

/// Is this a WebSocket upgrade request?
pub fn is_upgrade(req: &Request) -> bool {
    let has = |name: header::HeaderName, token: &str| {
        req.headers()
            .get_all(name)
            .iter()
            .filter_map(|v| v.to_str().ok())
            .flat_map(|v| v.split(','))
            .any(|t| t.trim().eq_ignore_ascii_case(token))
    };
    has(header::CONNECTION, "upgrade") && has(header::UPGRADE, "websocket")
}

/// Why a WebSocket handshake was refused.
#[derive(Debug, Clone, Copy)]
pub struct Refused {
    pub status: StatusCode,
    pub reason: &'static str,
}

/// Answer the handshake. Returns the `101 Switching Protocols` response to send, and a future
/// that yields the WebSocket once hyper has handed the connection over (spawn it).
pub fn accept(
    req: Request,
) -> Result<(Response, impl Future<Output = anyhow::Result<WebSocket>> + Send + 'static), Refused> {
    accept_limited(req, 16 << 20)
}

/// [`accept`], with one message (and one frame) bounded to `max_message` bytes: a peer that sends
/// more is disconnected.
pub fn accept_limited(
    req: Request,
    max_message: usize,
) -> Result<(Response, impl Future<Output = anyhow::Result<WebSocket>> + Send + 'static), Refused> {
    let bad = |reason: &'static str| Refused { status: StatusCode::BAD_REQUEST, reason };
    if !is_upgrade(&req) {
        return Err(bad("expected a WebSocket upgrade"));
    }
    if req.headers().get(header::SEC_WEBSOCKET_VERSION).and_then(|v| v.to_str().ok()) != Some("13") {
        return Err(bad("unsupported WebSocket version"));
    }
    let Some(key) = req.headers().get(header::SEC_WEBSOCKET_KEY) else {
        return Err(bad("missing Sec-WebSocket-Key"));
    };
    let accept = tungstenite::handshake::derive_accept_key(key.as_bytes());
    let mut resp = http::Response::new(empty());
    *resp.status_mut() = StatusCode::SWITCHING_PROTOCOLS;
    let h = resp.headers_mut();
    h.insert(header::UPGRADE, http::HeaderValue::from_static("websocket"));
    h.insert(header::CONNECTION, http::HeaderValue::from_static("Upgrade"));
    if let Ok(v) = http::HeaderValue::from_str(&accept) {
        h.insert(header::SEC_WEBSOCKET_ACCEPT, v);
    }
    let upgrade = async move {
        let upgraded = hyper::upgrade::on(req).await.map_err(|e| anyhow::anyhow!("upgrade: {e}"))?;
        let ws = async_tungstenite::WebSocketStream::from_raw_socket(
            FuturesIo(upgraded),
            tungstenite::protocol::Role::Server,
            Some(limits(max_message)),
        )
        .await;
        Ok(ws)
    };
    Ok((resp, upgrade))
}

fn limits(max_message: usize) -> WebSocketConfig {
    WebSocketConfig::default().max_message_size(Some(max_message)).max_frame_size(Some(max_message))
}

/// The connection under a client WebSocket: plain TCP, or TLS over it.
pub enum ClientIo {
    Plain(RtFuturesIo),
    Tls(Box<FuturesIo<TlsStream<RtFuturesIo>>>),
}

impl futures::io::AsyncRead for ClientIo {
    fn poll_read(self: Pin<&mut Self>, cx: &mut Context<'_>, buf: &mut [u8]) -> Poll<std::io::Result<usize>> {
        match self.get_mut() {
            Self::Plain(io) => Pin::new(io).poll_read(cx, buf),
            Self::Tls(io) => Pin::new(io.as_mut()).poll_read(cx, buf),
        }
    }
}

impl futures::io::AsyncWrite for ClientIo {
    fn poll_write(self: Pin<&mut Self>, cx: &mut Context<'_>, buf: &[u8]) -> Poll<std::io::Result<usize>> {
        match self.get_mut() {
            Self::Plain(io) => Pin::new(io).poll_write(cx, buf),
            Self::Tls(io) => Pin::new(io.as_mut()).poll_write(cx, buf),
        }
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        match self.get_mut() {
            Self::Plain(io) => Pin::new(io).poll_flush(cx),
            Self::Tls(io) => Pin::new(io.as_mut()).poll_flush(cx),
        }
    }

    fn poll_close(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        match self.get_mut() {
            Self::Plain(io) => Pin::new(io).poll_close(cx),
            Self::Tls(io) => Pin::new(io.as_mut()).poll_close(cx),
        }
    }
}

/// A client-side WebSocket.
pub type ClientSocket = async_tungstenite::WebSocketStream<ClientIo>;

/// Open a WebSocket to `url` (`ws://`, `wss://`, or `http(s)://` for the same) over kiln's own TCP
/// and TLS setup, trusting `tls`'s roots. tungstenite does the client's half of RFC 6455 as it does
/// the server's: the handshake (a fresh `Sec-WebSocket-Key`, the server's `Sec-WebSocket-Accept`
/// checked), masked frames, fragments, ping / pong and close. One message (and one frame) may hold
/// up to `max_message` bytes; a server that sends more is disconnected.
pub async fn connect(tls: &ClientTls, url: &str, max_message: usize) -> anyhow::Result<ClientSocket> {
    let uri: http::Uri = url.parse().map_err(|e| anyhow::anyhow!("bad url {url}: {e}"))?;
    let secure = match uri.scheme_str() {
        Some("wss" | "https") => true,
        Some("ws" | "http") => false,
        other => anyhow::bail!("unsupported scheme {other:?} in {url}"),
    };
    let host = uri.host().ok_or_else(|| anyhow::anyhow!("no host in {url}"))?;
    let host = host.trim_start_matches('[').trim_end_matches(']');
    let port = uri.port_u16().unwrap_or(if secure { 443 } else { 80 });
    let authority = uri.authority().map_or(host, http::uri::Authority::as_str);
    let path = uri.path_and_query().map_or("/", http::uri::PathAndQuery::as_str);
    let tcp = super::io::connect(host, port).await?;
    let io = if secure {
        let stream = TlsStream::connect(RtFuturesIo::new(tcp), tls.config()?, super::tls::server_name(host)?)
            .await
            .map_err(|e| anyhow::anyhow!("TLS to {host}: {e}"))?;
        ClientIo::Tls(Box::new(FuturesIo(stream)))
    } else {
        ClientIo::Plain(RtFuturesIo::new(tcp))
    };
    let target = format!("{}://{authority}{path}", if secure { "wss" } else { "ws" });
    let (ws, _) = async_tungstenite::client_async_with_config(target, io, Some(limits(max_message)))
        .await
        .map_err(|e| anyhow::anyhow!("WebSocket handshake with {url}: {e}"))?;
    Ok(ws)
}

/// Text frame helper.
pub fn text(s: impl Into<String>) -> Message {
    Message::text(s.into())
}

/// Binary frame helper.
pub fn binary(b: impl Into<bytes::Bytes>) -> Message {
    Message::binary(b.into())
}

#[cfg(test)]
mod tests {
    use std::io::{Read as _, Write as _};
    use std::net::{TcpListener, TcpStream};
    use std::time::Duration;

    use futures::StreamExt as _;

    use super::*;

    /// A WebSocket server written by hand on a thread: answers the handshake (with the accept key
    /// RFC 6455 asks for, or a wrong one), then runs `script` on the raw connection.
    fn raw_server(
        wrong_accept: bool,
        script: impl FnOnce(&mut TcpStream) -> anyhow::Result<()> + Send + 'static,
    ) -> anyhow::Result<(u16, std::thread::JoinHandle<anyhow::Result<()>>)> {
        let listener = TcpListener::bind("127.0.0.1:0")?;
        let port = listener.local_addr()?.port();
        let server = std::thread::spawn(move || -> anyhow::Result<()> {
            let (mut s, _) = listener.accept()?;
            s.set_read_timeout(Some(Duration::from_secs(10)))?;
            let mut head = Vec::new();
            let mut byte = [0u8; 1];
            while !head.ends_with(b"\r\n\r\n") {
                s.read_exact(&mut byte)?;
                head.extend_from_slice(&byte);
            }
            let head = String::from_utf8(head)?;
            anyhow::ensure!(head.starts_with("GET /api/agents/tunnel HTTP/1.1\r\n"), "request line: {head}");
            let key = head
                .lines()
                .find_map(|l| {
                    let (name, value) = l.split_once(':')?;
                    name.trim().eq_ignore_ascii_case("sec-websocket-key").then(|| value.trim().to_owned())
                })
                .ok_or_else(|| anyhow::anyhow!("no Sec-WebSocket-Key in {head}"))?;
            let accept = if wrong_accept {
                tungstenite::handshake::derive_accept_key(b"not the key")
            } else {
                tungstenite::handshake::derive_accept_key(key.as_bytes())
            };
            write!(s, "HTTP/1.1 101 Switching Protocols\r\nUpgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Accept: {accept}\r\n\r\n")?;
            script(&mut s)
        });
        Ok((port, server))
    }

    /// One frame from the client: (FIN, opcode, masked, payload unmasked).
    fn read_frame(s: &mut TcpStream) -> anyhow::Result<(bool, u8, bool, Vec<u8>)> {
        let mut head = [0u8; 2];
        s.read_exact(&mut head)?;
        let [b0, b1] = head;
        let len = match b1 & 0x7f {
            126 => {
                let mut l = [0u8; 2];
                s.read_exact(&mut l)?;
                u64::from(u16::from_be_bytes(l))
            }
            127 => {
                let mut l = [0u8; 8];
                s.read_exact(&mut l)?;
                u64::from_be_bytes(l)
            }
            n => u64::from(n),
        };
        let masked = b1 & 0x80 != 0;
        let mut key = [0u8; 4];
        if masked {
            s.read_exact(&mut key)?;
        }
        let mut payload = vec![0u8; usize::try_from(len)?];
        s.read_exact(&mut payload)?;
        for (b, k) in payload.iter_mut().zip(key.iter().cycle()) {
            *b ^= k;
        }
        Ok((b0 & 0x80 != 0, b0 & 0x0f, masked, payload))
    }

    /// A server frame (unmasked), written `chunk` bytes at a time with a pause after each, so the
    /// client's reads return pieces of it.
    fn write_frame(s: &mut TcpStream, fin: bool, opcode: u8, payload: &[u8], chunk: usize) -> anyhow::Result<()> {
        let mut f = vec![if fin { 0x80 } else { 0 } | opcode];
        match payload.len() {
            n if n < 126 => f.push(u8::try_from(n)?),
            n if n <= 0xffff => {
                f.push(126);
                f.extend_from_slice(&u16::try_from(n)?.to_be_bytes());
            }
            n => {
                f.push(127);
                f.extend_from_slice(&u64::try_from(n)?.to_be_bytes());
            }
        }
        f.extend_from_slice(payload);
        for piece in f.chunks(chunk.max(1)) {
            s.write_all(piece)?;
            s.flush()?;
            std::thread::sleep(Duration::from_millis(1));
        }
        Ok(())
    }

    fn tls() -> anyhow::Result<ClientTls> {
        ClientTls::new(crate::http::client::Roots::WebPki, None)
    }

    #[test]
    fn client_frames_are_masked_and_split_reads_reassemble() -> anyhow::Result<()> {
        let big: Vec<u8> = (0..1400u32).map(|i| u8::try_from(i % 251).unwrap_or(0)).collect();
        let sent = big.clone();
        let (port, server) = raw_server(false, move |s| {
            // The client's frame is masked; unmasked, it is what the client sent.
            let (fin, op, masked, payload) = read_frame(s)?;
            anyhow::ensure!(fin && op == 2 && masked && payload == b"datagram one", "{fin} {op} {masked} {payload:?}");
            // A binary message in 7-byte pieces.
            write_frame(s, true, 2, &sent, 7)?;
            // A message in three fragments with a ping between the first two, all byte by byte.
            write_frame(s, false, 2, b"frag", 1)?;
            write_frame(s, true, 9, b"p1", 1)?;
            write_frame(s, false, 0, b"men", 1)?;
            write_frame(s, true, 0, b"ted", 1)?;
            // The client answers the ping with a masked pong of the same payload.
            let (fin, op, masked, payload) = read_frame(s)?;
            anyhow::ensure!(fin && op == 10 && masked && payload == b"p1", "pong: {fin} {op} {masked} {payload:?}");
            // And a close with a masked close.
            write_frame(s, true, 8, &1000u16.to_be_bytes(), 64)?;
            let (_, op, masked, payload) = read_frame(s)?;
            anyhow::ensure!(op == 8 && masked && payload.get(..2) == Some(&1000u16.to_be_bytes()[..]), "close: {op} {masked} {payload:?}");
            Ok(())
        })?;
        crate::rt::block_on(async move {
            let mut ws = connect(&tls()?, &format!("ws://127.0.0.1:{port}/api/agents/tunnel"), 4096).await?;
            ws.send(binary(b"datagram one".to_vec())).await?;
            let mut got = Vec::new();
            while let Some(m) = ws.next().await {
                let m = m?;
                let close = matches!(m, Message::Close(_));
                got.push(m);
                if close {
                    break;
                }
            }
            // Drives the close reply out; the stream then ends.
            while ws.next().await.is_some() {}
            assert_eq!(got.len(), 4, "{got:?}");
            assert!(matches!(got.first(), Some(Message::Binary(b)) if b.as_ref() == big.as_slice()));
            assert!(matches!(got.get(1), Some(Message::Ping(p)) if p.as_ref() == b"p1"));
            assert!(matches!(got.get(2), Some(Message::Binary(b)) if b.as_ref() == b"fragmented"));
            assert!(matches!(got.get(3), Some(Message::Close(Some(c))) if u16::from(c.code) == 1000));
            Ok(())
        })?;
        server.join().map_err(|_| anyhow::anyhow!("server thread panicked"))?
    }

    #[test]
    fn client_checks_the_accept_key_and_the_message_bound() -> anyhow::Result<()> {
        // A server that answers with the wrong Sec-WebSocket-Accept is refused.
        let (port, server) = raw_server(true, |_| Ok(()))?;
        crate::rt::block_on(async move {
            let r = connect(&tls()?, &format!("http://127.0.0.1:{port}/api/agents/tunnel"), 4096).await;
            assert!(r.is_err(), "a wrong accept key was taken");
            Ok(())
        })?;
        server.join().map_err(|_| anyhow::anyhow!("server thread panicked"))??;

        // A message over the bound ends the connection instead of being buffered.
        let (port, server) = raw_server(false, |s| write_frame(s, true, 2, &[0u8; 2000], 4096))?;
        crate::rt::block_on(async move {
            let mut ws = connect(&tls()?, &format!("ws://127.0.0.1:{port}/api/agents/tunnel"), 1500).await?;
            assert!(matches!(ws.next().await, Some(Err(_))), "an oversized message was taken");
            Ok(())
        })?;
        server.join().map_err(|_| anyhow::anyhow!("server thread panicked"))?
    }
}
